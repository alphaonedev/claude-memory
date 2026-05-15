// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 G-PHASE-E-4 (issue #709) — `verify-forensic-bundle` and
//! `verify-reflection-chain` exit-code hardening.
//!
//! Pre-#709, both verbs returned `Ok(1)` on a verification failure. `1`
//! is the same code the shell uses for "command not found" / generic
//! failure / unwrap panic, so under `set -e` and friends an audit
//! script could not distinguish "verification failed" from "the CLI
//! itself crashed".
//!
//! `verify-signed-events-chain` already returns `0`/`1`; the intent of
//! #709 was to upgrade BOTH `verify-forensic-bundle` and
//! `verify-reflection-chain` to exit code **2** on verification
//! failure (the conventional "syntactically-correct command, but the
//! contents don't pass verification" code). The reports now also
//! carry a top-line `ok: bool` field on the JSON wire shape so
//! external scripts can `jq '.ok'` instead of recomputing the
//! predicate from sub-fields.
//!
//! Tests below pin:
//!
//! 1. `verify-reflection-chain`: tampered Ed25519 signature surfaces
//!    `exit code 2` and `ok: false` in the JSON wire shape.
//! 2. `verify-reflection-chain`: clean chain still exits `0` with
//!    `ok: true`.
//! 3. `verify-reflection-chain`: a chain that exceeds its governance
//!    cap also exits `2`.
//! 4. `verify-forensic-bundle`: tampered file inside a bundle surfaces
//!    `exit code 2`.

use ai_memory::cli::CliOutput;
use ai_memory::cli::verify as cli_verify;
use ai_memory::db;
use ai_memory::forensic::bundle as fb;
use ai_memory::identity::keypair as kp_mod;
use ai_memory::identity::sign;
use ai_memory::models::{Memory, MemoryKind, Tier};
use chrono::Utc;
use rusqlite::params;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn open_test_db(tmp: &TempDir) -> (rusqlite::Connection, std::path::PathBuf) {
    let db_path = tmp.path().join("ai-memory.db");
    let conn = db::open(&db_path).expect("db::open");
    (conn, db_path)
}

fn insert_mem(conn: &rusqlite::Connection, ns: &str, depth: i32, kind: MemoryKind) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: id.clone(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: format!("t-{depth}"),
        content: format!("c-{depth}"),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth: depth,
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    db::insert(conn, &mem).expect("insert");
    id
}

fn link_unsigned(conn: &rusqlite::Connection, src: &str, tgt: &str) {
    conn.execute(
        "INSERT OR IGNORE INTO memory_links \
         (source_id, target_id, relation, created_at, attest_level) \
         VALUES (?1, ?2, 'reflects_on', ?3, 'unsigned')",
        params![src, tgt, Utc::now().to_rfc3339()],
    )
    .expect("link_unsigned");
}

fn insert_signed_reflects_on(
    conn: &rusqlite::Connection,
    src: &str,
    tgt: &str,
    agent: &ai_memory::identity::keypair::AgentKeypair,
) -> Vec<u8> {
    let now = Utc::now().to_rfc3339();
    let link = sign::SignableLink {
        src_id: src,
        dst_id: tgt,
        relation: "reflects_on",
        observed_by: Some(&agent.agent_id),
        valid_from: Some(&now),
        valid_until: None,
    };
    let sig = sign::sign(agent, &link).expect("sign");
    conn.execute(
        "INSERT OR IGNORE INTO memory_links \
         (source_id, target_id, relation, created_at, valid_from, \
          signature, observed_by, attest_level) \
         VALUES (?1, ?2, 'reflects_on', ?3, ?3, ?4, ?5, 'self_signed')",
        params![src, tgt, now, sig, agent.agent_id],
    )
    .expect("insert signed link");
    sig
}

/// Attach a `max_reflection_depth` governance policy to `ns`.
fn set_cap(conn: &rusqlite::Connection, ns: &str, cap: u32) {
    let now = Utc::now().to_rfc3339();
    let policy = ai_memory::models::GovernancePolicy {
        max_reflection_depth: Some(cap),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        auto_classify_kind: None,
        ..ai_memory::models::GovernancePolicy::default()
    };
    let metadata = serde_json::json!({
        "governance": serde_json::to_value(&policy).unwrap()
    });
    let std_mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("_standard:{ns}"),
        content: "cap memory".into(),
        tags: vec!["_namespace_standard".into()],
        priority: 5,
        confidence: 1.0,
        source: "test".into(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    };
    let id = db::insert(conn, &std_mem).expect("insert std");
    db::set_namespace_standard(conn, ns, &id, None).expect("set std");
}

/// Drive `cli::verify::run` end-to-end against an on-disk DB and
/// return `(exit_code, stdout_string)`.
fn run_verify_reflection(db_path: &Path, mem_id: &str, format: &str) -> (i32, String) {
    let args = cli_verify::VerifyChainArgs {
        memory_id: mem_id.to_string(),
        format: format.to_string(),
        include_signed_events: false,
    };
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let code = {
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        cli_verify::run(db_path, &args, &mut out).expect("run")
    };
    (code, String::from_utf8(stdout).unwrap())
}

// ---------------------------------------------------------------------------
// verify-reflection-chain
// ---------------------------------------------------------------------------

#[test]
fn reflection_chain_tampered_signature_exits_2_and_ok_false() {
    let tmp = TempDir::new().unwrap();
    let keys_tmp = TempDir::new().unwrap();
    let (conn, db_path) = open_test_db(&tmp);

    let agent = kp_mod::generate("phase-e-4-tampered").expect("gen keypair");
    kp_mod::save(&agent, keys_tmp.path()).expect("save keypair");

    let d0 = insert_mem(&conn, "tamper-ns", 0, MemoryKind::Observation);
    let d1 = insert_mem(&conn, "tamper-ns", 1, MemoryKind::Reflection);
    let mut sig = insert_signed_reflects_on(&conn, &d1, &d0, &agent);

    // Flip the first byte of the signature — invalidates the Ed25519
    // verification while keeping the row otherwise well-formed.
    sig[0] ^= 0x01;
    conn.execute(
        "UPDATE memory_links SET signature = ?1 \
         WHERE source_id = ?2 AND target_id = ?3 AND relation = 'reflects_on'",
        params![sig, d1, d0],
    )
    .expect("tamper signature");
    drop(conn);

    // Point the key dir at the test keys so the verifier can find
    // the pubkey for `agent.agent_id`.
    unsafe {
        std::env::set_var("AI_MEMORY_KEY_DIR", keys_tmp.path());
    }
    let (code, stdout) = run_verify_reflection(&db_path, &d1, "json");
    unsafe {
        std::env::remove_var("AI_MEMORY_KEY_DIR");
    }

    assert_eq!(
        code, 2,
        "tampered signature must surface exit code 2 (#709); got {code}, stdout={stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(v["ok"], false, "ok must be false on tampered chain: {v}");
    assert!(v["edges_failed"].as_u64().unwrap_or(0) >= 1);
}

#[test]
fn reflection_chain_clean_exits_0_and_ok_true() {
    let tmp = TempDir::new().unwrap();
    let (conn, db_path) = open_test_db(&tmp);
    let d0 = insert_mem(&conn, "clean-ns", 0, MemoryKind::Observation);
    let d1 = insert_mem(&conn, "clean-ns", 1, MemoryKind::Reflection);
    let d2 = insert_mem(&conn, "clean-ns", 2, MemoryKind::Reflection);
    link_unsigned(&conn, &d2, &d1);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let (code, stdout) = run_verify_reflection(&db_path, &d2, "json");
    assert_eq!(code, 0, "clean chain must exit 0; stdout={stdout}");
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(v["ok"], true, "ok must be true on clean chain: {v}");
}

#[test]
fn reflection_chain_exceeded_cap_exits_2() {
    let tmp = TempDir::new().unwrap();
    let (conn, db_path) = open_test_db(&tmp);

    // Cap = 1, memory at depth 2 violates it.
    set_cap(&conn, "oob-ns", 1);
    let d0 = insert_mem(&conn, "oob-ns", 0, MemoryKind::Observation);
    let d1 = insert_mem(&conn, "oob-ns", 1, MemoryKind::Reflection);
    let d2 = insert_mem(&conn, "oob-ns", 2, MemoryKind::Reflection);
    link_unsigned(&conn, &d2, &d1);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let (code, stdout) = run_verify_reflection(&db_path, &d2, "json");
    assert_eq!(
        code, 2,
        "exceeded-cap chain must exit 2 (#709); got {code}, stdout={stdout}"
    );
    let v: serde_json::Value = serde_json::from_str(&stdout).expect("parse JSON report");
    assert_eq!(v["ok"], false, "ok must be false on exceeded cap: {v}");
    assert_eq!(v["bounded_status"], "exceeded_cap");
}

// ---------------------------------------------------------------------------
// verify-forensic-bundle
// ---------------------------------------------------------------------------

#[test]
fn forensic_bundle_tampered_file_exits_2() {
    // Build a real bundle, tamper a file inside the tarball, and verify.
    // Pre-#709 this exited 1; post-#709 it must exit 2.
    let tmp = TempDir::new().unwrap();
    let (conn, db_path) = open_test_db(&tmp);
    let d0 = insert_mem(&conn, "fb-ns", 0, MemoryKind::Observation);
    let d1 = insert_mem(&conn, "fb-ns", 1, MemoryKind::Reflection);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let bundle_path = tmp.path().join("bundle.tar");
    let export_args = fb::ExportForensicBundleArgs {
        memory_id: d1.clone(),
        output: Some(bundle_path.clone()),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
    };
    {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let code = fb::run_export(&db_path, &export_args, &mut out).expect("export");
        assert_eq!(code, 0, "export must succeed on clean DB");
    }
    assert!(bundle_path.exists(), "bundle must exist after export");

    // Tamper one file inside the tarball without re-signing the manifest.
    let bytes = std::fs::read(&bundle_path).expect("read bundle");
    let mut files = fb::read_ustar(&bytes).expect("parse bundle tar");
    let target_path = files
        .keys()
        .find(|k| k.starts_with("memories/"))
        .expect("at least one memory entry")
        .clone();
    files.insert(target_path.clone(), b"tampered".to_vec());
    let repacked = fb::pack_to_vec(&files).expect("repack");
    std::fs::write(&bundle_path, &repacked).expect("write tampered bundle");

    let verify_args = fb::VerifyForensicBundleArgs {
        bundle_path: bundle_path.clone(),
    };
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let code = fb::run_verify(&verify_args, &mut out).expect("run_verify");
    let stdout_s = String::from_utf8(stdout).unwrap();
    assert_eq!(
        code, 2,
        "tampered bundle must exit 2 (#709); got {code}, stdout={stdout_s}"
    );
    // The report's JSON should also carry ok=false.
    // The CLI prints the JSON before the human-readable summary line.
    let json_end = stdout_s
        .find("verification FAILED")
        .unwrap_or(stdout_s.len());
    let json_text = &stdout_s[..json_end];
    let v: serde_json::Value = serde_json::from_str(json_text.trim()).expect("parse JSON");
    assert_eq!(v["ok"], false, "ok must be false on tampered bundle: {v}");
}

#[test]
fn forensic_bundle_clean_exits_0() {
    // Confirm the clean path still exits 0 — i.e. #709 only touched
    // the failure-side exit code.
    let tmp = TempDir::new().unwrap();
    let (conn, db_path) = open_test_db(&tmp);
    let d0 = insert_mem(&conn, "fb-clean-ns", 0, MemoryKind::Observation);
    let d1 = insert_mem(&conn, "fb-clean-ns", 1, MemoryKind::Reflection);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let bundle_path = tmp.path().join("clean.tar");
    let export_args = fb::ExportForensicBundleArgs {
        memory_id: d1.clone(),
        output: Some(bundle_path.clone()),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
    };
    {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let code = fb::run_export(&db_path, &export_args, &mut out).expect("export");
        assert_eq!(code, 0);
    }

    let verify_args = fb::VerifyForensicBundleArgs {
        bundle_path: bundle_path.clone(),
    };
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    let code = fb::run_verify(&verify_args, &mut out).expect("run_verify");
    assert_eq!(code, 0, "clean bundle must still exit 0 post-#709");
}
