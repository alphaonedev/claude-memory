// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Sandbox boundary: for each `AgentAction` variant, a rule that
//! refuses it → the substrate path refuses. Pins the symmetry
//! property that every variant has a working refusal path. This is
//! the load-bearing acceptance gate for issue #691 — without it,
//! one of the five kinds could silently no-op refusals.

use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store::{self, Rule};

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
             timestamp TEXT NOT NULL
         );",
    )
    .unwrap();
    conn
}

fn add(conn: &rusqlite::Connection, id: &str, kind: &str, matcher: &str) {
    rules_store::insert(
        conn,
        &Rule {
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
            attest_level: "unsigned".into(),
        },
    )
    .unwrap();
}

#[test]
fn bash_variant_can_be_refused() {
    let conn = fresh_conn();
    add(&conn, "B1", "bash", r#"{"command_regex":"forbidden"}"#);
    let a = AgentAction::Bash {
        command: "forbidden command".into(),
        cwd: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn filesystem_write_variant_can_be_refused() {
    let conn = fresh_conn();
    add(&conn, "F1", "filesystem_write", r#"{"glob":"/tmp/**"}"#);
    let a = AgentAction::FilesystemWrite {
        path: "/tmp/x".into(),
        byte_estimate: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn network_request_variant_can_be_refused() {
    let conn = fresh_conn();
    add(&conn, "N1", "network_request", r#"{"host":"evil.x"}"#);
    let a = AgentAction::NetworkRequest {
        host: "evil.x".into(),
        scheme: "https".into(),
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert!(matches!(d, Decision::Refuse { .. }));
}

#[test]
fn process_spawn_variant_can_be_refused() {
    let conn = fresh_conn();
    add(
        &conn,
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
    let conn = fresh_conn();
    add(&conn, "C1", "custom", r#"{"kind":"deploy_prod"}"#);
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
    let conn = fresh_conn();
    add(&conn, "F1", "filesystem_write", r#"{"glob":"/tmp/**"}"#);
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
    let conn = fresh_conn();
    add(&conn, "F1", "filesystem_write", r#"{"glob":"/tmp/**"}"#);
    let a = AgentAction::FilesystemWrite {
        path: "/Users/me/safe.txt".into(),
        byte_estimate: None,
    };
    let d = check_agent_action(&conn, "a", &a).unwrap();
    assert_eq!(d, Decision::Allow);
}
