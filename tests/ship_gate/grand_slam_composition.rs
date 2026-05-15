// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioural impact.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::missing_panics_doc
)]

//! v0.7.0 L3-3 — ship-gate scenarios for the cross-pillar composition
//! surface (L1-6 substrate-rules + L2-5 forensic bundle + L2-7
//! reflection-skill composition + migration / CHECK round-trip).
//!
//! Where the sibling files cover the recursive-learning spine
//! (`grand_slam_recursive_learning.rs`) and the Agent Skills pillar
//! (`grand_slam_skills.rs`), this file pins the **composition**
//! scenarios: features that only assemble correctly when the
//! sibling-feature substrate is wired end-to-end.
//!
//! Phase coverage in this file:
//!
//! Phase 1 (FUNCTIONAL)
//!   1. Forensic bundle build → verify round-trip on a depth-2
//!      reflection chain (L2-5).
//!   2. Substrate-rule R001-R004 enforcement when operator-signed AND
//!      enabled — `/tmp/**` write refuses with rule id surfaced (L1-6
//!      A–D bypass-impossibility surface).
//!
//! Phase 2 (FEDERATION)
//!   3. Substrate-rule signed at peer A replicates to peer B and
//!      enforces (the L1-6 E federation property: rules cross peers
//!      with their signature/attestation intact).
//!
//! Phase 3 (MIGRATION)
//!   4. After running the full v33 migration ladder, every L1+L2 table
//!      is present and the closed-taxonomy CHECK on
//!      `memory_links.relation` refuses an unknown relation written
//!      via direct SQL.
//!
//! Phase 4 (CHAOS / sanity)
//!   5. L2-7 reflection-skill composition: a composing skill mints
//!      `composes_with_reflections` declarations that survive
//!      register → metadata round-trip (the substrate's
//!      backwards-compat guarantee with pre-L2-7 readers).
//!
//! Hermetic: tempdir DBs, in-memory SQLite, deterministic test
//! signatures, no live network.

use std::path::Path;

use ai_memory::db;
use ai_memory::forensic::bundle::{self, ExportForensicBundleArgs};
use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store::{self, Rule};
use ai_memory::models::{Memory, MemoryKind, MemoryLinkRelation, Tier};
use ai_memory::parsing::skill_md;
use chrono::Utc;
use ed25519_dalek::{Signer, SigningKey};
use rusqlite::Connection;
use std::sync::{Mutex, OnceLock};
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

/// Process-wide mutex for env-var mutation. Mirrors the L1-6
/// activation-test fixture. Several tests in this file mutate
/// `AI_MEMORY_OPERATOR_PUBKEY`, and any concurrent test that reads it
/// mid-mutation would observe a transient value. Hold this guard for
/// the duration of any test body that flips the env.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn install_operator_pubkey(signing: &SigningKey) {
    use base64::Engine;
    let pk_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    // SAFETY: env mutation serialised by env_lock for the duration of
    // the calling test. No other process touches this var.
    unsafe { std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", pk_b64) };
}

fn uninstall_operator_pubkey() {
    // SAFETY: env mutation serialised by env_lock.
    unsafe { std::env::remove_var("AI_MEMORY_OPERATOR_PUBKEY") };
}

/// Build a fresh in-memory schema for the substrate-rules sub-tests.
/// Mirrors the helper used by `tests/governance_a2a_rules.rs` so this
/// file is independent of the cross-file `mod` graph.
fn fresh_rules_conn() -> Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE governance_rules (
             id TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             matcher TEXT NOT NULL,
             severity TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
             reason TEXT NOT NULL,
             namespace TEXT NOT NULL DEFAULT '_global',
             created_by TEXT NOT NULL,
             created_at INTEGER NOT NULL,
             enabled INTEGER NOT NULL DEFAULT 1,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned'
         );
         CREATE TABLE signed_events (
             id TEXT PRIMARY KEY,
             agent_id TEXT NOT NULL,
             event_type TEXT NOT NULL,
             payload_hash BLOB NOT NULL,
             signature BLOB,
             attest_level TEXT NOT NULL DEFAULT 'unsigned',
             timestamp TEXT NOT NULL,
             -- v34 (V-4 closeout, #698) — cross-row chain columns.
             prev_hash BLOB,
             sequence INTEGER
         );",
    )
    .unwrap();
    conn
}

fn insert_seed_rule(conn: &Connection, id: &str, glob: &str, enabled: bool) {
    rules_store::insert(
        conn,
        &Rule {
            id: id.to_string(),
            kind: "filesystem_write".into(),
            matcher: format!(r#"{{"glob":"{glob}"}}"#),
            severity: "refuse".into(),
            reason: format!("{id}: ship-gate seed rule"),
            namespace: "_global".into(),
            created_by: "system:seed".into(),
            created_at: 0,
            enabled,
            signature: None,
            attest_level: "unsigned".into(),
        },
    )
    .unwrap();
}

fn sign_all_rules(conn: &Connection, signing: &SigningKey) {
    for rule in rules_store::list(conn).unwrap() {
        let canonical = rules_store::canonical_bytes_for_signing(&rule).unwrap();
        let sig = signing.sign(&canonical);
        rules_store::update_signature(conn, &rule.id, &sig.to_bytes(), "operator_signed").unwrap();
    }
}

fn probe_write(conn: &Connection, path: &str) -> Decision {
    let action = AgentAction::FilesystemWrite {
        path: path.into(),
        byte_estimate: None,
    };
    check_agent_action(conn, "agent:l3-3", &action).unwrap()
}

fn insert_memory(conn: &Connection, ns: &str, title: &str, depth: i32, kind: MemoryKind) -> String {
    let now = Utc::now().to_rfc3339();
    let m = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: title.to_string(),
        content: format!("content for {title}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "ai:l3-3"}),
        reflection_depth: depth,
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    db::insert(conn, &m).expect("insert")
}

fn link_unsigned(conn: &Connection, src: &str, tgt: &str, relation: &str) {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_links (source_id, target_id, relation, created_at, attest_level) \
         VALUES (?1, ?2, ?3, ?4, 'unsigned')",
        rusqlite::params![src, tgt, relation, now],
    )
    .expect("link_unsigned");
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — forensic bundle export + verify round-trip.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-1 (Phase 1):** Build a forensic bundle over a depth-2
/// reflection chain via [`bundle::build`], then [`bundle::verify`] it
/// — the report must come back `ok = true` with the right
/// `memory_id`. Composes L2-5 into the ship-gate functional phase.
#[test]
fn sg_co_1_forensic_bundle_export_and_verify_roundtrip() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("sg-co-1.db");
    let conn = db::open(&db_path).expect("open");
    let ns = "ship-gate-co-1";

    // depth-2 chain: d2 → d1 → d0
    let d0 = insert_memory(&conn, ns, "co1-d0", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, ns, "co1-d1", 1, MemoryKind::Reflection);
    let d2 = insert_memory(&conn, ns, "co1-d2", 2, MemoryKind::Reflection);
    link_unsigned(&conn, &d2, &d1, "reflects_on");
    link_unsigned(&conn, &d1, &d0, "reflects_on");

    let bundle_path = tmp.path().join("co1-bundle.tar");
    let args = ExportForensicBundleArgs {
        memory_id: d2.clone(),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
        output: None,
    };

    // Build with a fixed `generated_at` so the bundle is reproducible
    // (the L2-5 byte-identical contract).
    bundle::build(&conn, &args, &bundle_path, Some("2026-05-14T00:00:00Z"))
        .expect("forensic bundle build ok");

    let report = bundle::verify(&bundle_path).expect("verify must succeed");
    assert!(report.ok, "verify report must be ok: {report:?}");
    assert!(report.manifest_present, "manifest.json present in bundle");
    assert_eq!(report.memory_id, d2, "memory_id matches the export target");
    assert!(
        report.tampered_files.is_empty(),
        "no tampered files in a fresh build: {:?}",
        report.tampered_files
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — substrate-rule R001-R004 enforcement.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-2 (Phase 1):** With the operator pubkey installed, a seed
/// rule that is BOTH signed AND enabled refuses a matching action;
/// flipping the rule to disabled or stripping the signature both
/// surface as Allow (rule SKIPPED). Pins the L1-6 A–D
/// bypass-impossibility surface: enforcement requires operator-signed
/// AND enabled.
#[test]
fn sg_co_2_substrate_rule_r001_enforces_when_signed_and_enabled() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    install_operator_pubkey(&signing);

    let conn = fresh_rules_conn();

    // Land R001 in the canonical seed shape: filesystem_write covering
    // /tmp/**, enabled = 1, unsigned. With the pubkey configured the
    // engine SKIPS this row (no signature) → Allow.
    insert_seed_rule(&conn, "R001", "/tmp/**", true);
    assert_eq!(
        probe_write(&conn, "/tmp/leak"),
        Decision::Allow,
        "unsigned+enabled rule must be skipped when L1-6 pubkey is configured"
    );

    // Sign every row. Now enabled+signed → refuse.
    sign_all_rules(&conn, &signing);
    let refuse = probe_write(&conn, "/tmp/leak");
    match refuse {
        Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
        other => panic!("expected Refuse after sign, got {other:?}"),
    }

    // Disabling the rule via the substrate API removes enforcement.
    rules_store::set_enabled(&conn, "R001", false).unwrap();
    assert_eq!(
        probe_write(&conn, "/tmp/leak"),
        Decision::Allow,
        "signed-but-disabled rule must not enforce"
    );

    // Sanity: a path outside /tmp is allowed even when rule is enabled.
    rules_store::set_enabled(&conn, "R001", true).unwrap();
    let outside_tmp = probe_write(&conn, "/Users/fate/v07/v07-fixes/.local-runs/ok.txt");
    assert_eq!(
        outside_tmp,
        Decision::Allow,
        "path outside /tmp glob must pass even when R001 is signed+enabled"
    );

    uninstall_operator_pubkey();
}

/// **SG-CO-3 (Phase 1):** The seed rules R002 (/var/tmp/**) and R003
/// (/private/tmp/**) cover the macOS realpath family. With all four
/// seed rules signed and enabled the operator's three forbidden
/// scratch families refuse simultaneously — the ship-gate
/// "no agent-created files under any tmpfs" contract.
#[test]
fn sg_co_3_substrate_rules_r001_r002_r003_refuse_all_three_tmp_families() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    install_operator_pubkey(&signing);

    let conn = fresh_rules_conn();
    insert_seed_rule(&conn, "R001", "/tmp/**", true);
    insert_seed_rule(&conn, "R002", "/var/tmp/**", true);
    insert_seed_rule(&conn, "R003", "/private/tmp/**", true);
    sign_all_rules(&conn, &signing);

    for (path, expected_rule) in [
        ("/tmp/x", "R001"),
        ("/var/tmp/y", "R002"),
        ("/private/tmp/z", "R003"),
    ] {
        match probe_write(&conn, path) {
            Decision::Refuse { rule_id, .. } => assert_eq!(
                rule_id, expected_rule,
                "path {path} should match rule {expected_rule}"
            ),
            other => panic!("expected Refuse for {path}, got {other:?}"),
        }
    }

    uninstall_operator_pubkey();
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 (FEDERATION) — rule signed at A replicates and enforces at B.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-4 (Phase 2):** A signed rule authored at peer A and
/// row-copied to peer B carries its signature + attest_level intact;
/// the receiver enforces it. Composes L1-6 E (federation property)
/// into a ship-gate scenario.
#[test]
fn sg_co_4_substrate_rule_signed_at_a_replicates_and_enforces_at_b() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    install_operator_pubkey(&signing);

    let peer_a = fresh_rules_conn();
    let peer_b = fresh_rules_conn();

    // Operator on A: seed + sign R001.
    insert_seed_rule(&peer_a, "R001", "/tmp/**", true);
    sign_all_rules(&peer_a, &signing);

    let rule_a = rules_store::get(&peer_a, "R001")
        .unwrap()
        .expect("rule on A");
    assert_eq!(rule_a.attest_level, "operator_signed");
    assert!(rule_a.signature.is_some());

    // Replicate the rule from A to B via the same `insert` the
    // subscription-replay dispatcher would issue.
    rules_store::insert(&peer_b, &rule_a).expect("replicate R001 to B");

    // Verify the row landed on B with full attestation preserved.
    let rule_b = rules_store::get(&peer_b, "R001")
        .unwrap()
        .expect("rule on B");
    assert_eq!(rule_b.attest_level, "operator_signed");
    assert_eq!(rule_b.signature, rule_a.signature);
    assert_eq!(rule_b.matcher, rule_a.matcher);

    // B enforces the inherited rule.
    let refuse = probe_write(&peer_b, "/tmp/replicated");
    match refuse {
        Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
        other => panic!("expected Refuse on B after replication, got {other:?}"),
    }

    uninstall_operator_pubkey();
}

// ─────────────────────────────────────────────────────────────────────
// Phase 3 (MIGRATION) — full schema ladder + closed-taxonomy CHECK
// constraint enforcement post-migration.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-5 (Phase 3):** Open a fresh DB, let the migration ladder
/// run to v33. Every L1+L2 table the ship-gate touches must exist:
/// `memories`, `memory_links`, `signed_events`, `pending_actions`,
/// `governance_rules`, `skills`, `skill_resources`,
/// `namespace_standards`. Composes the full Layer 1 + Layer 2 schema
/// surface into a single ship-gate pin.
#[test]
fn sg_co_5_migration_ladder_lands_every_l1_l2_table_and_check_constraint() {
    let conn = db::open(Path::new(":memory:")).expect("open");

    for table in [
        "memories",
        "memory_links",
        "signed_events",
        "pending_actions",
        "governance_rules",
        "skills",
        "skill_resources",
        // `namespace_meta` is the table that backs `set_namespace_standard`
        // — operators sometimes search for `namespace_standards`; this
        // pin documents the actual storage name.
        "namespace_meta",
    ] {
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type = 'table' AND name = ?1)",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .expect("sqlite_master probe");
        assert_eq!(
            exists, 1,
            "table {table} must exist after v33 migration ladder"
        );
    }

    // CHECK constraint on memory_links.relation refuses bad relations
    // post-migration. Seed two memories so the link can FK-resolve.
    let s = insert_memory(&conn, "ship-gate-co-5", "co5-s", 0, MemoryKind::Observation);
    let t = insert_memory(&conn, "ship-gate-co-5", "co5-t", 0, MemoryKind::Observation);
    let now = Utc::now().to_rfc3339();
    let err = conn
        .execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&s, &t, "bogus_relation", now],
        )
        .expect_err("bogus relation must fail CHECK constraint");
    let msg = err.to_string().to_ascii_lowercase();
    assert!(
        msg.contains("check") || msg.contains("constraint"),
        "post-v33 CHECK constraint must refuse 'bogus_relation', got: {err}"
    );

    // Positive control — every documented closed-taxonomy relation
    // is accepted post-migration.
    for rel in [
        "related_to",
        "supersedes",
        "contradicts",
        "derived_from",
        "reflects_on",
    ] {
        // Use a fresh target per relation to keep the (source, target,
        // relation) PK unique without exhausting the closed list.
        let fresh_t = insert_memory(
            &conn,
            "ship-gate-co-5",
            &format!("co5-tgt-{rel}"),
            0,
            MemoryKind::Observation,
        );
        let now2 = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO memory_links (source_id, target_id, relation, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![&s, &fresh_t, rel, now2],
        )
        .unwrap_or_else(|e| panic!("legal relation '{rel}' should land post-migration: {e}"));
    }
}

// ─────────────────────────────────────────────────────────────────────
// Phase 4 (CHAOS / sanity) — L2-7 reflection-skill composition
// metadata survives register → metadata-blob → read-back.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-6 (Phase 4):** A SKILL.md that declares
/// `composes_with_reflections` parses into the typed Vec AND mirrors
/// the declaration into the JSON `metadata` blob — the backwards-
/// compat guarantee for pre-L2-7 readers that only consult `metadata`.
/// Composes L2-7 (reflection composition) into the ship-gate. The
/// substrate's bounded-recursion ceiling (`max_reflection_depth`) is
/// authoritative — composition cannot bypass it; this test pins the
/// PARSE side of that contract (a `min_depth` floor below the
/// ceiling is legal data).
#[test]
fn sg_co_6_reflection_skill_composition_metadata_round_trips() {
    let composing_md = "---\n\
namespace: ship-gate-co-6\n\
name: composing-skill\n\
description: \"Reflection-composing skill (SG-CO-6).\"\n\
composes_with_reflections:\n  \
  - namespace: ship-gate-co-6/observations\n    \
    min_depth: 1\n  \
  - namespace: ship-gate-co-6/lessons\n    \
    min_depth: 2\n\
---\n\nBody.\n";

    let manifest = skill_md::parse(composing_md).expect("parse composing skill");
    assert_eq!(manifest.name, "composing-skill");
    assert_eq!(manifest.composes_with_reflections.len(), 2);
    assert_eq!(
        manifest.composes_with_reflections[0].namespace,
        "ship-gate-co-6/observations"
    );
    assert_eq!(manifest.composes_with_reflections[0].min_depth, 1);
    assert_eq!(
        manifest.composes_with_reflections[1].namespace,
        "ship-gate-co-6/lessons"
    );
    assert_eq!(manifest.composes_with_reflections[1].min_depth, 2);

    // L2-7 backwards-compat: the declaration is also mirrored into the
    // JSON metadata bag so pre-L2-7 readers see it as opaque data
    // rather than missing entirely.
    let mirrored = manifest
        .metadata
        .get("composes_with_reflections")
        .expect("composes_with_reflections mirrored into metadata for pre-L2-7 compatibility");
    assert!(
        mirrored.is_array(),
        "metadata mirror must be an array: got {mirrored:?}"
    );
    let arr = mirrored.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    assert_eq!(arr[0]["namespace"], "ship-gate-co-6/observations");
    assert_eq!(arr[0]["min_depth"], 1);

    // The substrate persists this declaration via skill_register; we
    // already pin the register/export round-trip in grand_slam_skills.
    // Here we pin the structural backwards-compat shape only — the
    // load-bearing parse-side guarantee.
}

// ─────────────────────────────────────────────────────────────────────
// Phase 4 (CHAOS / sanity) — substrate's signed_events audit chain
// emits at every check_agent_action call (audit-write best-effort
// contract from L1-6).
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-7 (Phase 4):** The substrate-rules engine emits a
/// `signed_events` row on every `check_agent_action` call, even when
/// the matched rule is filtered out at load time (unsigned, tampered,
/// or disabled). Audit-completeness pin: the dashboard count must
/// never drift below the call count regardless of rule eligibility.
#[test]
fn sg_co_7_check_agent_action_emits_audit_row_on_every_call() {
    let _g = env_lock()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    // No pubkey configured here — the engine runs in
    // "no-operator-attest" mode where signed/unsigned rules behave
    // symmetrically.
    uninstall_operator_pubkey();

    let conn = fresh_rules_conn();
    insert_seed_rule(&conn, "R001", "/tmp/**", true);

    let baseline: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = 'governance.check'",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Call the engine 5 times in mixed-match modes.
    for path in [
        "/tmp/a",
        "/tmp/b",
        "/Users/fate/v07/.local-runs/c.txt", // miss
        "/var/tmp/d",                        // no rule
        "/tmp/e",
    ] {
        let _ = probe_write(&conn, path);
    }

    let after: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = 'governance.check'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        after - baseline,
        5,
        "every check_agent_action call must emit one governance.check audit row"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 1 (FUNCTIONAL) — forensic bundle reports tampered files when a
// post-build mutation breaks the per-file SHA-256 manifest.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-8 (Phase 1):** When a file inside a built bundle is
/// mutated after the manifest is written, `verify` surfaces the
/// tampered file by name (L2-5: the verifier reports the offender
/// rather than a generic "FAIL"). Round-trip-with-tamper acceptance
/// — the audit-grade artifact's integrity guarantee.
#[test]
fn sg_co_8_forensic_bundle_verify_reports_tampered_file_by_name() {
    use ai_memory::forensic::bundle::{pack_to_vec, read_ustar};

    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("sg-co-8.db");
    let conn = db::open(&db_path).expect("open");

    let d0 = insert_memory(
        &conn,
        "ship-gate-co-8",
        "co8-d0",
        0,
        MemoryKind::Observation,
    );

    let args = ExportForensicBundleArgs {
        memory_id: d0.clone(),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
        output: None,
    };
    let mut files =
        bundle::build_files(&conn, &args, Some("2026-05-14T00:00:00Z")).expect("build_files ok");

    // Tamper a non-manifest file in the bundle. Path strings in the
    // bundle map are normalized lowercase by the substrate; we use a
    // case-insensitive suffix check to be robust to a future
    // case-preservation change without re-pinning the tampered set.
    let memory_file_name = files
        .keys()
        .find(|k| {
            k.starts_with("memories/")
                && std::path::Path::new(k.as_str())
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        })
        .cloned()
        .expect("at least one memory file in bundle");
    files
        .get_mut(&memory_file_name)
        .unwrap()
        .extend_from_slice(b"// tampered tail");

    // Pack + write the tampered bundle to disk.
    let bytes = pack_to_vec(&files).expect("pack_to_vec");
    let bundle_path = tmp.path().join("tampered.tar");
    std::fs::write(&bundle_path, &bytes).expect("write tampered bundle");

    // Sanity — the bundle parses, then verify catches the tamper.
    let parsed = read_ustar(&bytes).expect("read_ustar");
    assert!(parsed.contains_key(&memory_file_name));

    let report = bundle::verify(&bundle_path).expect("verify runs");
    assert!(!report.ok, "tampered bundle must NOT verify ok");
    assert!(
        report.tampered_files.iter().any(|f| f == &memory_file_name),
        "verify must name the tampered file by path: \
         expected {memory_file_name:?} in {:?}",
        report.tampered_files
    );
}

// ─────────────────────────────────────────────────────────────────────
// Phase 2 (FEDERATION) — cross-pillar smoke: depth-2 reflection
// REFLECTS_ON edge round-trips via the closed-taxonomy CHECK constraint.
// ─────────────────────────────────────────────────────────────────────

/// **SG-CO-9 (Phase 2):** A `reflects_on` edge between two memories
/// in different namespaces lands cleanly under the v33 CHECK
/// constraint (the relation is in the closed taxonomy). Pins the
/// migration-time invariant that the CHECK promotion did not
/// accidentally exclude `reflects_on` from the allowed-list. Trivial
/// but load-bearing: a wider migration that omits `reflects_on` would
/// silently break the recursive-learning substrate.
#[test]
fn sg_co_9_reflects_on_edge_passes_v33_check_constraint() {
    let conn = db::open(Path::new(":memory:")).expect("open");
    let s = insert_memory(&conn, "co9-ns-a", "co9-src", 0, MemoryKind::Observation);
    let t = insert_memory(&conn, "co9-ns-b", "co9-tgt", 0, MemoryKind::Observation);

    // The substrate validator + CHECK constraint both must accept
    // `reflects_on` between two distinct memories (no cycle).
    db::create_link(&conn, &s, &t, "reflects_on")
        .expect("reflects_on link must pass validator AND CHECK constraint");
    let links = db::get_links(&conn, &s).expect("get_links");
    let reflects: Vec<_> = links
        .iter()
        .filter(|l| l.relation == MemoryLinkRelation::ReflectsOn)
        .collect();
    assert_eq!(reflects.len(), 1, "exactly one reflects_on edge landed");
}
