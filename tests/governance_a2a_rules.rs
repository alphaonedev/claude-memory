// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Federation property: a rule added at peer A and replicated to
//! peer B is enforced at peer B. The full A2A federation replicator
//! is out of this commit's scope (operator wires up
//! `subscription_replay` for `governance_rules` separately); this
//! test models replication as a SQL row copy and verifies the
//! enforcement path on the receiving side is symmetric with the
//! authoring side.

use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store::{self, Rule};

mod common;
use common::*;

// Tests in this file mutate the process-wide
// `AI_MEMORY_OPERATOR_PUBKEY` env var so `resolve_operator_pubkey()`
// returns the in-test key rather than the host's on-disk
// `operator.key.pub`. `common::install_test_operator_key()` does the
// install + returns an `EnvVarGuard` whose drop restores prior state;
// the guard holds the shared `ENV_LOCK` so parallel tests in this
// binary don't race on the env var.

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

/// Replicate one rule from `peer_a` to `peer_b` by reading the row and
/// re-inserting. Mirrors the shape an A2A subscription dispatcher
/// would do — read source state, INSERT OR IGNORE on destination.
fn replicate_rule(peer_a: &rusqlite::Connection, peer_b: &rusqlite::Connection, id: &str) {
    let rule = rules_store::get(peer_a, id).unwrap().expect("source rule");
    rules_store::insert(peer_b, &rule).unwrap();
}

#[test]
fn rule_authored_at_peer_a_replicated_to_peer_b_enforces_at_b() {
    let (signing, _env_guard) = install_test_operator_key();
    let peer_a = fresh_conn();
    let peer_b = fresh_conn();

    // Peer A: operator adds R001 (no /tmp). Signed with the in-test
    // operator key so the production `enforced_rule_passes` filter
    // accepts it under L1-6 (pubkey resolved → signed rules required).
    let r001 = sign_rule(
        Rule {
            id: "R001".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/tmp/**"}"#.into(),
            severity: "refuse".into(),
            reason: "no /tmp".into(),
            namespace: "_global".into(),
            created_by: "operator".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "operator_signed".into(),
        },
        &signing,
    );
    let expected_sig = r001
        .signature
        .clone()
        .expect("sign_rule populates signature");
    rules_store::insert(&peer_a, &r001).unwrap();

    // Replication step.
    replicate_rule(&peer_a, &peer_b, "R001");

    // Peer B: rule is present with the same 64-byte signature.
    let on_b = rules_store::get(&peer_b, "R001").unwrap().unwrap();
    assert_eq!(on_b.id, "R001");
    assert_eq!(on_b.signature, Some(expected_sig));
    assert_eq!(on_b.attest_level, "operator_signed");

    // Peer B enforces: a /tmp write is refused.
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/foo".into(),
        byte_estimate: None,
    };
    let decision = check_agent_action(&peer_b, "agent:b", &action).unwrap();
    assert!(matches!(decision, Decision::Refuse { .. }));
}

#[test]
fn disabled_rule_at_peer_b_does_not_enforce_even_if_enabled_at_a() {
    let (signing, _env_guard) = install_test_operator_key();
    let peer_a = fresh_conn();
    let peer_b = fresh_conn();

    // Peer A: enabled rule. Signed with the in-test operator key so
    // the L1-6 enforcement filter accepts it on the peer-A refuse
    // assertion below (the disabled branch on peer B doesn't depend
    // on signing — the SQL `enabled = 0` filter short-circuits before
    // signature verification — but we sign here too for symmetry with
    // production replication semantics).
    let r002 = sign_rule(
        Rule {
            id: "R002".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/tmp/**"}"#.into(),
            severity: "refuse".into(),
            reason: "no /tmp".into(),
            namespace: "_global".into(),
            created_by: "operator".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "operator_signed".into(),
        },
        &signing,
    );
    rules_store::insert(&peer_a, &r002).unwrap();

    // Replicate, then disable on B (peer B's operator opts out).
    replicate_rule(&peer_a, &peer_b, "R002");
    rules_store::set_enabled(&peer_b, "R002", false).unwrap();

    // Peer B: disabled rule, write allowed.
    let action = AgentAction::FilesystemWrite {
        path: "/tmp/foo".into(),
        byte_estimate: None,
    };
    let decision = check_agent_action(&peer_b, "agent:b", &action).unwrap();
    assert_eq!(decision, Decision::Allow);

    // Peer A: still enabled, still refuses.
    let decision_a = check_agent_action(&peer_a, "agent:a", &action).unwrap();
    assert!(matches!(decision_a, Decision::Refuse { .. }));
}

#[test]
fn replication_preserves_signature_for_audit_chain() {
    let peer_a = fresh_conn();
    let peer_b = fresh_conn();

    let sig = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    rules_store::insert(
        &peer_a,
        &Rule {
            id: "R-sig".into(),
            kind: "bash".into(),
            matcher: r#"{"command_regex":"x"}"#.into(),
            severity: "refuse".into(),
            reason: "test".into(),
            namespace: "_global".into(),
            created_by: "operator".into(),
            created_at: 12345,
            enabled: true,
            signature: Some(sig.clone()),
            attest_level: "operator_signed".into(),
        },
    )
    .unwrap();

    replicate_rule(&peer_a, &peer_b, "R-sig");
    let on_b = rules_store::get(&peer_b, "R-sig").unwrap().unwrap();
    assert_eq!(on_b.signature, Some(sig));
    assert_eq!(on_b.attest_level, "operator_signed");
    assert_eq!(on_b.created_at, 12345);
}
