// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 #697 — Ed25519-signed forensic audit log regression test.
//!
//! Pins the chain + signature contract that `ai-memory audit verify
//! --since <ISO_DATE>` consumes:
//!
//! 1. Record decisions to a fresh forensic directory.
//! 2. Run `verify_since` → must report `total_lines == N` and no
//!    failures.
//! 3. Tamper with one line on disk → `verify_since` must surface a
//!    failure (Signature OR ChainBreak depending on which field
//!    was tampered).

#![allow(
    clippy::missing_panics_doc,
    clippy::doc_markdown,
    clippy::redundant_closure_for_method_calls
)]

use std::path::PathBuf;
use std::sync::Mutex;

use ai_memory::governance::audit as forensic;
use chrono::Utc;
use ed25519_dalek::SigningKey;
use rand_core::OsRng;
use tempfile::TempDir;

/// The forensic sink is process-wide. Serialise so concurrent
/// integration tests do not race the sink swap-out.
fn test_lock() -> &'static Mutex<()> {
    use std::sync::OnceLock;
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn local_runs_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".local-runs")
        .join("forensic-test")
}

fn fresh_dir() -> TempDir {
    let root = local_runs_root();
    std::fs::create_dir_all(&root).ok();
    tempfile::tempdir_in(&root).expect("tempdir under .local-runs")
}

fn fresh_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

#[test]
fn signed_chain_records_and_verifies() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = fresh_dir();
    let key = fresh_key();
    let pubkey = key.verifying_key();
    forensic::shutdown();
    forensic::init(dir.path(), Some(key)).expect("init forensic sink");

    forensic::record_decision(
        "ai:test-author",
        "allow",
        "bash",
        "",
        serde_json::json!({"command": "ls"}),
    );
    forensic::record_decision(
        "ai:test-author",
        "refuse",
        "bash",
        "R042",
        serde_json::json!({"command": "rm -rf /", "reason": "matches deny-rule R042"}),
    );
    forensic::record_decision(
        "ai:test-author",
        "warn",
        "network_request",
        "R055",
        serde_json::json!({"host": "evil.example.com"}),
    );
    forensic::shutdown();

    let since = Utc::now().format("%Y-%m-%d").to_string();
    let report = forensic::verify_since(dir.path(), &since, Some(&pubkey)).expect("verify");
    assert!(
        report.first_failure.is_none(),
        "fresh signed chain must verify clean, got: {:?}",
        report.first_failure
    );
    assert_eq!(report.total_lines, 3, "expected 3 lines, got: {report:?}");
    assert_eq!(
        report.unsigned_lines, 0,
        "expected 0 unsigned, got: {report:?}"
    );
}

#[test]
fn tampered_line_fails_verify() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = fresh_dir();
    let key = fresh_key();
    let pubkey = key.verifying_key();
    forensic::shutdown();
    forensic::init(dir.path(), Some(key)).expect("init");
    forensic::record_decision(
        "ai:author-a",
        "refuse",
        "bash",
        "R001",
        serde_json::json!({"command": "shutdown now"}),
    );
    forensic::record_decision(
        "ai:author-b",
        "allow",
        "bash",
        "",
        serde_json::json!({"command": "ls"}),
    );
    forensic::shutdown();

    let date = Utc::now().format("%Y-%m-%d").to_string();
    let path = dir.path().join(format!("forensic-{date}.jsonl"));
    let body = std::fs::read_to_string(&path).expect("read forensic file");
    let tampered = body.replacen("\"ai:author-a\"", "\"ai:evil-actor\"", 1);
    std::fs::write(&path, tampered).expect("write tampered file");

    let report = forensic::verify_since(dir.path(), &date, Some(&pubkey)).expect("verify");
    let failure = report
        .first_failure
        .expect("tampered chain MUST fail verify");
    assert!(
        matches!(
            failure.kind,
            forensic::VerifyFailureKind::Signature | forensic::VerifyFailureKind::ChainBreak
        ),
        "tamper must surface Signature OR ChainBreak, got: {:?}",
        failure.kind
    );
}

#[test]
fn unsigned_chain_verifies_with_no_key_required() {
    let _g = test_lock().lock().unwrap_or_else(|e| e.into_inner());
    let dir = fresh_dir();
    forensic::shutdown();
    forensic::init(dir.path(), None).expect("init unsigned");
    for i in 0..3 {
        forensic::record_decision("ai:nokey", "allow", "bash", "", serde_json::json!({"i": i}));
    }
    forensic::shutdown();

    let since = Utc::now().format("%Y-%m-%d").to_string();
    let report = forensic::verify_since(dir.path(), &since, None).expect("verify unsigned");
    assert!(report.first_failure.is_none());
    assert_eq!(report.total_lines, 3);
    assert_eq!(report.unsigned_lines, 3);
}
