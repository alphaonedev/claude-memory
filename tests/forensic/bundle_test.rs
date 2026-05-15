// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-5 (issue #670) — acceptance tests for the forensic
//! evidence bundle CLI surface.
//!
//! Three load-bearing acceptance criteria from the issue:
//!
//! 1. Depth-2 chain bundle, verify → succeeds (build-then-verify
//!    round-trip on a real DB exercises the entire pipeline).
//! 2. Tampered file in bundle → reports the tampered file (the
//!    verifier names the offender, not just "FAIL").
//! 3. Bundle reproducible (byte-identical mod timestamp).
//!
//! All scratch lives in per-test `TempDir`s — never `/tmp` (project
//! hard rule). `AI_MEMORY_NO_CONFIG=1` is set on every spawned
//! subprocess per the standard CLI test convention.

#![allow(clippy::doc_markdown)]

use ai_memory::cli::CliOutput;
use ai_memory::db;
use ai_memory::forensic::bundle::{
    self, ExportForensicBundleArgs, VerifyForensicBundleArgs, pack_to_vec, read_ustar,
};
use ai_memory::models::{Memory, MemoryKind, Tier};
use assert_cmd::Command;
use chrono::Utc;
use rusqlite::params;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────

/// Build the `ai-memory --db <path>` command with `AI_MEMORY_NO_CONFIG=1`.
fn ai_memory_cmd(db_path: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db_path.to_str().unwrap()]);
    cmd
}

fn open_db(p: &std::path::Path) -> rusqlite::Connection {
    db::open(p).expect("db::open")
}

fn insert_memory(conn: &rusqlite::Connection, ns: &str, depth: i32, kind: MemoryKind) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let mem = Memory {
        id: id.clone(),
        tier: Tier::Mid,
        namespace: ns.to_string(),
        title: format!("t-{depth}"),
        content: format!("c-{depth}"),
        reflection_depth: depth,
        created_at: now.clone(),
        updated_at: now,
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        ..Default::default()
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

// ─────────────────────────────────────────────────────────────────────
// Acceptance criterion #1 — depth-2 chain bundle verifies.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn depth2_chain_bundle_verifies_via_cli() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    // depth-2 chain: d2 -> d1 -> d0
    let d0 = insert_memory(&conn, "fb", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, "fb", 1, MemoryKind::Reflection);
    let d2 = insert_memory(&conn, "fb", 2, MemoryKind::Reflection);
    link_unsigned(&conn, &d2, &d1);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let bundle_path = tmp.path().join("bundle.tar");
    ai_memory_cmd(&db_path)
        .args([
            "export-forensic-bundle",
            "--memory-id",
            &d2,
            "--include-reflections",
            "--output",
            bundle_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    assert!(bundle_path.exists(), "tarball must exist after export");

    let assert = ai_memory_cmd(&db_path)
        .args(["verify-forensic-bundle", bundle_path.to_str().unwrap()])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("verification OK"),
        "expected 'verification OK' in stdout, got: {stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Acceptance criterion #2 — tampered file is named in the report.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn tampered_file_in_bundle_is_named_by_verifier() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    let d0 = insert_memory(&conn, "fb-tamper", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, "fb-tamper", 1, MemoryKind::Reflection);
    link_unsigned(&conn, &d1, &d0);
    drop(conn);

    let bundle_path = tmp.path().join("bundle.tar");
    ai_memory_cmd(&db_path)
        .args([
            "export-forensic-bundle",
            "--memory-id",
            &d1,
            "--include-reflections",
            "--output",
            bundle_path.to_str().unwrap(),
        ])
        .assert()
        .success();

    // Tamper: rewrite a memory file body without re-signing the
    // manifest. Re-pack and overwrite the on-disk bundle.
    let bytes = std::fs::read(&bundle_path).expect("read");
    let mut files = read_ustar(&bytes).expect("parse");
    let target_path = files
        .keys()
        .find(|k| k.starts_with("memories/"))
        .expect("at least one memory entry")
        .clone();
    files.insert(target_path.clone(), b"tampered".to_vec());
    let repacked = pack_to_vec(&files).expect("repack");
    std::fs::write(&bundle_path, &repacked).expect("write tampered tar");

    let assert = ai_memory_cmd(&db_path)
        .args(["verify-forensic-bundle", bundle_path.to_str().unwrap()])
        .assert()
        .failure();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert!(
        stdout.contains("verification FAILED"),
        "expected verification FAILED line, got: {stdout}"
    );
    assert!(
        stdout.contains(&target_path),
        "verifier must name the tampered file path '{target_path}' \
         in its report; got stdout:\n{stdout}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Acceptance criterion #3 — bundle reproducibility mod timestamp.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn bundle_byte_identical_modulo_timestamp() {
    // Two builds against the same DB with the same pinned
    // `generated_at` must produce byte-identical archives. The
    // substrate-level `build_files` lets us pin the timestamp; the
    // CLI passes `None` (which fills in `Utc::now()` and is the only
    // legitimate source of non-determinism per the acceptance spec).
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    let d0 = insert_memory(&conn, "fb-repro", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, "fb-repro", 1, MemoryKind::Reflection);
    let d2 = insert_memory(&conn, "fb-repro", 2, MemoryKind::Reflection);
    link_unsigned(&conn, &d2, &d1);
    link_unsigned(&conn, &d1, &d0);

    let args = ExportForensicBundleArgs {
        memory_id: d2.clone(),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
        output: None,
    };
    let files_a = bundle::build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build a");
    let files_b = bundle::build_files(&conn, &args, Some("2026-01-01T00:00:00Z")).expect("build b");
    let bytes_a = bundle::pack_to_vec(&files_a).expect("pack a");
    let bytes_b = bundle::pack_to_vec(&files_b).expect("pack b");

    assert_eq!(
        bytes_a, bytes_b,
        "byte-identical mod timestamp is the L2-5 acceptance criterion"
    );

    // Cross-check the "byte-identical mod timestamp" boundary: when
    // the only thing that differs between two builds is the pinned
    // `generated_at`, the divergence is bounded to exactly two files
    // — `manifest.json` and `verification.json` — and every other
    // entry is byte-identical. The embedded chain report inherits the
    // bundle's timestamp by design (so an auditor sees a single
    // "bundle-generated-at" instant rather than two clock readings
    // that drifted across the function-call boundary).
    let files_c = bundle::build_files(&conn, &args, Some("2026-06-06T06:06:06Z")).expect("build c");
    let allow_to_vary: std::collections::HashSet<&str> =
        ["manifest.json", "verification.json"].into_iter().collect();
    for (path, body) in &files_a {
        if allow_to_vary.contains(path.as_str()) {
            continue;
        }
        let other = files_c.get(path).expect("same file set");
        assert_eq!(
            other, body,
            "non-manifest file '{path}' diverged across pinned-timestamp rebuilds"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// In-process substrate exercise — drives `forensic::bundle` against
// `CliOutput<Vec<u8>>` to assert on the stdout shape directly.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn run_verify_emits_structured_report_on_clean_bundle() {
    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);
    let d0 = insert_memory(&conn, "fb-rep", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, "fb-rep", 1, MemoryKind::Reflection);
    link_unsigned(&conn, &d1, &d0);

    let bundle_path = tmp.path().join("bundle.tar");
    let args = ExportForensicBundleArgs {
        memory_id: d1.clone(),
        include_reflections: true,
        include_transcripts: false,
        include_atomisation_chain: true,
        output: Some(bundle_path.clone()),
    };
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    {
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        bundle::run_export(&db_path, &args, &mut out).expect("export");
    }

    let verify_args = VerifyForensicBundleArgs {
        bundle_path: bundle_path.clone(),
    };
    let mut vstdout = Vec::<u8>::new();
    let mut vstderr = Vec::<u8>::new();
    let exit = {
        let mut out = CliOutput::from_std(&mut vstdout, &mut vstderr);
        bundle::run_verify(&verify_args, &mut out).expect("verify")
    };
    let s = String::from_utf8(vstdout).unwrap();
    assert_eq!(exit, 0);
    assert!(s.contains("\"ok\": true"), "expected ok=true in JSON: {s}");
    assert!(s.contains("verification OK"), "expected OK suffix: {s}");
}

#[test]
fn transcripts_flag_includes_replay_union() {
    use ai_memory::transcripts::storage as ts;

    let tmp = TempDir::new().unwrap();
    let db_path = tmp.path().join("ai-memory.db");
    let conn = open_db(&db_path);

    let d0 = insert_memory(&conn, "fb-tx", 0, MemoryKind::Observation);
    let d1 = insert_memory(&conn, "fb-tx", 1, MemoryKind::Reflection);
    link_unsigned(&conn, &d1, &d0);

    let t = ts::store(&conn, "fb-tx", "hello world transcript", None).expect("store transcript");
    ts::link_transcript(&conn, &d0, &t.id, None, None).expect("link transcript");

    let bundle_path = tmp.path().join("bundle.tar");
    let args = ExportForensicBundleArgs {
        memory_id: d1.clone(),
        include_reflections: true,
        include_transcripts: true,
        include_atomisation_chain: true,
        output: Some(bundle_path.clone()),
    };
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
    bundle::run_export(&db_path, &args, &mut out).expect("export");

    let bytes = std::fs::read(&bundle_path).expect("read");
    let files = read_ustar(&bytes).expect("parse");
    let tx_meta_key = format!("transcripts/{}.json", t.id);
    let tx_content_key = format!("transcripts/{}.content", t.id);
    assert!(
        files.contains_key(&tx_meta_key),
        "transcript metadata must be bundled when --include-transcripts is set"
    );
    assert!(
        files.contains_key(&tx_content_key),
        "transcript content must be bundled when --include-transcripts is set"
    );
}
