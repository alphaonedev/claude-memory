// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Sandbox boundary: for each `AgentAction` variant, a rule that
//! refuses it → the substrate path refuses. Pins the symmetry
//! property that every variant has a working refusal path. This is
//! the load-bearing acceptance gate for issue #691 — without it,
//! one of the five kinds could silently no-op refusals.

use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store::{self, Rule};
use ed25519_dalek::{Signer, SigningKey};

mod common;
use common::*;

// Same hermetic-test pattern used by sibling governance test files
// (a2a_rules, agent_action, deferred_log_audit): production
// `enforced_rule_passes` drops unsigned rules when an operator
// pubkey resolves. Each test calls `install_test_operator_key()` (in
// `common`) to install a per-test keypair in
// `AI_MEMORY_OPERATOR_PUBKEY` so assertions hold regardless of host
// state. The returned `EnvVarGuard` holds the shared `ENV_LOCK`
// across the modify-test-restore region.

fn fresh_conn() -> rusqlite::Connection {
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

fn add(conn: &rusqlite::Connection, signing: &SigningKey, id: &str, kind: &str, matcher: &str) {
    let mut rule = Rule {
        id: id.into(),
        kind: kind.into(),
        matcher: matcher.into(),
        severity: "refuse".into(),
        reason: format!("{id}: test refuse"),
        namespace: "_global".into(),
        created_by: "test".into(),
        created_at: 0,
        enabled: true,
        signature: None,
        attest_level: "operator_signed".into(),
    };
    let canonical =
        rules_store::canonical_bytes_for_signing(&rule).expect("canonical_bytes_for_signing");
    rule.signature = Some(signing.sign(&canonical).to_bytes().to_vec());
    rules_store::insert(conn, &rule).unwrap();
}

#[test]
fn bash_variant_can_be_refused() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "B1",
        "bash",
        r#"{"command_regex":"forbidden"}"#,
    );
    let a = AgentAction::Bash {
        command: "forbidden command".into(),
        cwd: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn filesystem_write_variant_can_be_refused() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "F1",
        "filesystem_write",
        r#"{"glob":"/tmp/**"}"#,
    );
    let a = AgentAction::FilesystemWrite {
        path: "/tmp/x".into(),
        byte_estimate: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn network_request_variant_can_be_refused() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "N1",
        "network_request",
        r#"{"host":"evil.x"}"#,
    );
    let a = AgentAction::NetworkRequest {
        host: "evil.x".into(),
        scheme: "https".into(),
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn process_spawn_variant_can_be_refused() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "P1",
        "process_spawn",
        r#"{"binary":"forbidden-bin"}"#,
    );
    let a = AgentAction::ProcessSpawn {
        binary: "forbidden-bin".into(),
        args: vec![],
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn custom_variant_can_be_refused() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(&conn, &signing, "C1", "custom", r#"{"kind":"deploy_prod"}"#);
    let a = AgentAction::Custom {
        custom_kind: "deploy_prod".into(),
        payload: serde_json::json!({}),
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

/// Glob `**` crosses path separators — confirms the engine reuses
/// the substrate-wide glob vocabulary from
/// `crate::governance::glob_matches` so namespace-pattern rules and
/// agent-action rules cannot drift.
#[test]
fn filesystem_write_double_star_glob_matches_subdir() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "F1",
        "filesystem_write",
        r#"{"glob":"/tmp/**"}"#,
    );
    let a = AgentAction::FilesystemWrite {
        path: "/tmp/deep/nested/file.log".into(),
        byte_estimate: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

/// Sibling path outside the glob is allowed.
#[test]
fn filesystem_write_outside_glob_allowed() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add(
        &conn,
        &signing,
        "F1",
        "filesystem_write",
        r#"{"glob":"/tmp/**"}"#,
    );
    let a = AgentAction::FilesystemWrite {
        path: "/Users/me/safe.txt".into(),
        byte_estimate: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert_eq!(d, Decision::Allow);
}
