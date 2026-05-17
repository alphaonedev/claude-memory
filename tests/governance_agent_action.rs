// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the substrate-level agent-action rules
//! engine (issue #691). Covers:
//!
//! * Every [`AgentAction`] matcher type (Bash / `FilesystemWrite` /
//!   `NetworkRequest` / `ProcessSpawn` / Custom)
//! * Decision routing (first-refusal wins; warn-without-refuse)
//! * `signed_events` row emission on every check (the audit chain)
//!
//! Sibling test files cover the singleton (`governance_singleton.rs`),
//! immutability (`governance_immutability.rs`), sandbox boundary
//! (`governance_sandbox_boundary.rs`), and A2A replication
//! (`governance_a2a_rules.rs`) properties.

use ai_memory::governance::agent_action::{
    AgentAction, Decision, GOVERNANCE_CHECK_EVENT_TYPE, check_agent_action,
};
use ai_memory::governance::rules_store::{self, Rule};
use ed25519_dalek::{Signer, SigningKey};

mod common;
use common::*;

// Same pattern as `tests/governance_a2a_rules.rs`: production
// `enforced_rule_passes` drops any rule whose `attest_level !=
// "operator_signed"` whenever `resolve_operator_pubkey()` returns a
// key (env var OR `~/Library/Application Support/ai-memory/operator
// .key.pub` on macOS). Tests install their own keypair via the env
// var (see `common::install_test_operator_key`) so the assertions
// hold regardless of host state.

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

fn add_rule(
    conn: &rusqlite::Connection,
    signing: &SigningKey,
    id: &str,
    kind: &str,
    matcher: &str,
    severity: &str,
) {
    let mut rule = Rule {
        id: id.into(),
        kind: kind.into(),
        matcher: matcher.into(),
        severity: severity.into(),
        reason: format!("{id}: test refusal"),
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

fn count_audit_rows(conn: &rusqlite::Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
        rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn bash_matcher_substring_match_refuses() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R-bash",
        "bash",
        r#"{"command_regex":"rm -rf /"}"#,
        "refuse",
    );
    let action = AgentAction::Bash {
        command: "rm -rf / --no-preserve-root".into(),
        cwd: None,
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert!(matches!(decision, Decision::Refuse { .. }));
}

#[test]
fn filesystem_write_glob_match_refuses() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R001",
        "filesystem_write",
        r#"{"glob":"/tmp/**"}"#,
        "refuse",
    );
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/foo/bar.log".into(),
        byte_estimate: Some(1024),
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    match decision {
        Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
        _ => panic!("expected refuse, got {decision:?}"),
    }
}

#[test]
fn network_request_host_match_refuses() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R-evil",
        "network_request",
        r#"{"host":"malware.example"}"#,
        "refuse",
    );
    let action = AgentAction::NetworkRequest {
        host: "malware.example".into(),
        scheme: "https".into(),
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert!(matches!(decision, Decision::Refuse { .. }));
}

#[test]
fn process_spawn_binary_match_refuses() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R-cargo",
        "process_spawn",
        r#"{"binary":"cargo"}"#,
        "refuse",
    );
    let action = AgentAction::ProcessSpawn {
        binary: "cargo".into(),
        args: vec!["build".into()],
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert!(matches!(decision, Decision::Refuse { .. }));
}

#[test]
fn custom_kind_match_refuses() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R-deploy",
        "custom",
        r#"{"kind":"deploy_prod"}"#,
        "refuse",
    );
    let action = AgentAction::Custom {
        custom_kind: "deploy_prod".into(),
        payload: serde_json::json!({"region": "us-east-1"}),
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert!(matches!(decision, Decision::Refuse { .. }));
}

#[test]
fn first_refuse_short_circuits_remaining_rules() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    // R-A is a Warn; R-B is the first Refuse; R-C is also a Refuse.
    // The engine should return R-B's refusal, not R-C's.
    add_rule(
        &conn,
        &signing,
        "R-A",
        "bash",
        r#"{"command_regex":"rm"}"#,
        "warn",
    );
    add_rule(
        &conn,
        &signing,
        "R-B",
        "bash",
        r#"{"command_regex":"rm"}"#,
        "refuse",
    );
    add_rule(
        &conn,
        &signing,
        "R-C",
        "bash",
        r#"{"command_regex":"rm"}"#,
        "refuse",
    );
    let action = AgentAction::Bash {
        command: "rm -rf /".into(),
        cwd: None,
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    match decision {
        Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R-B"),
        _ => panic!("expected refuse, got {decision:?}"),
    }
}

#[test]
fn warn_without_refuse_returns_warn() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "W001",
        "bash",
        r#"{"command_regex":"sudo"}"#,
        "warn",
    );
    let action = AgentAction::Bash {
        command: "sudo apt update".into(),
        cwd: None,
    };
    let decision = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert!(matches!(decision, Decision::Warn { .. }));
}

#[test]
fn each_check_emits_one_signed_event() {
    let conn = fresh_conn();
    let action = AgentAction::Bash {
        command: "echo hello".into(),
        cwd: None,
    };
    for _ in 0..5 {
        let _ = check_agent_action(&conn, "agent:t", &action).unwrap();
    }
    assert_eq!(count_audit_rows(&conn), 5);
}

#[test]
fn refusal_path_still_emits_signed_event() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = fresh_conn();
    add_rule(
        &conn,
        &signing,
        "R001",
        "filesystem_write",
        r#"{"glob":"/tmp/**"}"#,
        "refuse",
    );
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/x".into(),
        byte_estimate: None,
    };
    let _ = check_agent_action(&conn, "agent:t", &action).unwrap();
    assert_eq!(count_audit_rows(&conn), 1);
    // Audit row carries the agent_id.
    let agent_id: String = conn
        .query_row(
            "SELECT agent_id FROM signed_events WHERE event_type = ?1 LIMIT 1",
            rusqlite::params![GOVERNANCE_CHECK_EVENT_TYPE],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(agent_id, "agent:t");
}
