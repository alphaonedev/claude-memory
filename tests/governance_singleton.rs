// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Singleton property: 100 concurrent `check_agent_action` calls
//! against the same rule set return consistent decisions. Pins the
//! invariant that the engine has no hidden mutable state that could
//! produce divergent answers under contention.

use std::sync::{Arc, Mutex};
use std::thread;

use ai_memory::governance::agent_action::{AgentAction, Decision, check_agent_action};
use ai_memory::governance::rules_store::{self, Rule};
use ed25519_dalek::Signer;

mod common;
use common::*;

// Hermetic-test pattern: production `enforced_rule_passes` drops
// unsigned rules when an operator pubkey resolves (env or on-disk
// `operator.key.pub`). `install_test_operator_key()` (in `common`)
// generates a per-test keypair and installs it in
// `AI_MEMORY_OPERATOR_PUBKEY` so the assertion below sees the rule
// as enforced regardless of host state. The returned `EnvVarGuard`
// holds the shared `ENV_LOCK` for its lifetime so parallel tests
// don't race on the env var.

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

#[test]
fn hundred_concurrent_checks_against_same_rule_return_consistent_decisions() {
    let (signing, _env_guard) = install_test_operator_key();
    let conn = Arc::new(Mutex::new(fresh_conn()));
    {
        let c = conn.lock().unwrap();
        let mut rule = Rule {
            id: "R001".into(),
            kind: "filesystem_write".into(),
            matcher: r#"{"glob":"/tmp/**"}"#.into(),
            severity: "refuse".into(),
            reason: "no /tmp".into(),
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
        rules_store::insert(&c, &rule).unwrap();
    }

    let mut handles = Vec::new();
    for i in 0..100 {
        let conn_clone = Arc::clone(&conn);
        let h = thread::spawn(move || {
            let action = AgentAction::FilesystemWrite {
                path: "/tmp/foo".into(),
                byte_estimate: None,
            };
            let c = conn_clone.lock().unwrap();
            let agent_id = format!("agent:thread-{i}");
            check_agent_action(&c, &agent_id, &action).unwrap()
        });
        handles.push(h);
    }

    let decisions: Vec<Decision> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(decisions.len(), 100);
    // Every decision must be Refuse with rule_id=R001 — no allow,
    // no warn, no panic under contention.
    for d in &decisions {
        match d {
            Decision::Refuse { rule_id, .. } => assert_eq!(rule_id, "R001"),
            other => panic!("non-refuse decision under contention: {other:?}"),
        }
    }

    // 100 audit rows.
    let c = conn.lock().unwrap();
    let count: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = 'governance.check'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 100);
}

#[test]
fn hundred_concurrent_allow_checks_consistent() {
    let conn = Arc::new(Mutex::new(fresh_conn()));
    let mut handles = Vec::new();
    for i in 0..100 {
        let conn_clone = Arc::clone(&conn);
        let h = thread::spawn(move || {
            let action = AgentAction::Bash {
                command: format!("echo {i}"),
                cwd: None,
            };
            let c = conn_clone.lock().unwrap();
            check_agent_action(&c, "agent:t", &action).unwrap()
        });
        handles.push(h);
    }
    for h in handles {
        let d = h.join().unwrap();
        assert_eq!(d, Decision::Allow);
    }
}
