// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the v0.7.0 universal `AgentAction` wire-points
//! (issue #691 fold-1).
//!
//! Pins the refuse-with-rule-enabled behaviour for the four
//! daemon-side wire-point variants now consulting the
//! `governance::wire_check::GOVERNANCE_PRE_ACTION` hook:
//!
//! - `FilesystemWrite` — verified via direct hook installation
//! - `NetworkRequest` — verified via direct hook installation
//! - `ProcessSpawn` — verified via direct hook installation
//! - `Custom` (control case — already gated by `storage::GOVERNANCE_PRE_WRITE`)
//!
//! The `OnceLock` holding the hook is process-wide. Cargo runs each
//! integration-test crate in its own binary, so we own the install
//! for this binary's lifetime and route by the action's sentinel
//! fields to assert Allow + Refuse on every variant from one process.

use ai_memory::governance::agent_action::AgentAction;
use ai_memory::governance::wire_check::{self, GOVERNANCE_PRE_ACTION};

/// Install a routing hook that refuses any action whose distinguishing
/// field contains the sentinel `__refuse__`. Returns silently if the
/// `OnceLock` is already populated (cargo test binary may run this in
/// parallel with itself; only the first install wins, which is fine —
/// the hook shape is the same on every test).
fn install_routing_hook() {
    let _ = GOVERNANCE_PRE_ACTION.set(Box::new(|action: &AgentAction| match action {
        AgentAction::Bash { command, .. } if command.contains("__refuse__") => {
            Err("bash refuse".to_string())
        }
        AgentAction::FilesystemWrite { path, .. }
            if path.to_string_lossy().contains("__refuse__") =>
        {
            Err("filesystem_write refuse".to_string())
        }
        AgentAction::NetworkRequest { host, .. } if host.contains("__refuse__") => {
            Err("network_request refuse".to_string())
        }
        AgentAction::ProcessSpawn { binary, .. } if binary.contains("__refuse__") => {
            Err("process_spawn refuse".to_string())
        }
        AgentAction::Custom { custom_kind, .. } if custom_kind.contains("__refuse__") => {
            Err("custom refuse".to_string())
        }
        _ => Ok(()),
    }));
}

// ---------------------------------------------------------------------------
// FilesystemWrite — operator-rule-enabled refusal short-circuits writes
// ---------------------------------------------------------------------------

#[test]
fn filesystem_write_allow_when_no_rule_matches() {
    install_routing_hook();
    let action = AgentAction::FilesystemWrite {
        path: "/Users/agent/notes/safe.md".into(),
        byte_estimate: Some(256),
    };
    assert!(
        wire_check::check(&action).is_ok(),
        "non-sentinel path must allow"
    );
}

#[test]
fn filesystem_write_refuses_when_rule_matches() {
    install_routing_hook();
    let action = AgentAction::FilesystemWrite {
        path: "/scratch/__refuse__/x.txt".into(),
        byte_estimate: None,
    };
    let refusal = wire_check::check(&action).expect_err("expected refuse");
    assert_eq!(refusal.reason, "filesystem_write refuse");
}

#[test]
fn filesystem_write_anyhow_downcasts_to_governance_refusal() {
    install_routing_hook();
    let action = AgentAction::FilesystemWrite {
        path: "/__refuse__".into(),
        byte_estimate: None,
    };
    let e = wire_check::check_anyhow(&action).expect_err("expected refuse");
    let refusal = e
        .downcast_ref::<ai_memory::storage::GovernanceRefusal>()
        .expect("downcast to GovernanceRefusal");
    assert_eq!(refusal.reason, "filesystem_write refuse");
}

// ---------------------------------------------------------------------------
// NetworkRequest — outbound HTTPS refusal (federation, llm)
// ---------------------------------------------------------------------------

#[test]
fn network_request_allow_for_unmatched_host() {
    install_routing_hook();
    let action = AgentAction::NetworkRequest {
        host: "good.example.com".into(),
        scheme: "https".into(),
    };
    assert!(wire_check::check(&action).is_ok());
}

#[test]
fn network_request_refuses_when_rule_matches() {
    install_routing_hook();
    let action = AgentAction::NetworkRequest {
        host: "__refuse__.evil.example.com".into(),
        scheme: "https".into(),
    };
    let refusal = wire_check::check(&action).expect_err("expected refuse");
    assert_eq!(refusal.reason, "network_request refuse");
}

// ---------------------------------------------------------------------------
// ProcessSpawn — hooks::executor exec-mode + daemon-mode refusal
// ---------------------------------------------------------------------------

#[test]
fn process_spawn_allow_for_unmatched_binary() {
    install_routing_hook();
    let action = AgentAction::ProcessSpawn {
        binary: "cargo".into(),
        args: vec!["build".into()],
    };
    assert!(wire_check::check(&action).is_ok());
}

#[test]
fn process_spawn_refuses_when_rule_matches() {
    install_routing_hook();
    let action = AgentAction::ProcessSpawn {
        binary: "__refuse__-binary".into(),
        args: vec![],
    };
    let refusal = wire_check::check(&action).expect_err("expected refuse");
    assert_eq!(refusal.reason, "process_spawn refuse");
}

// ---------------------------------------------------------------------------
// Bash — reserved for harness-side PreToolUse (no daemon wire-point in fold-1)
// ---------------------------------------------------------------------------

#[test]
fn bash_action_routes_through_wire_check() {
    install_routing_hook();
    let allow = AgentAction::Bash {
        command: "ls -la".into(),
        cwd: None,
    };
    assert!(wire_check::check(&allow).is_ok());
    let refuse = AgentAction::Bash {
        command: "echo __refuse__".into(),
        cwd: None,
    };
    let refusal = wire_check::check(&refuse).expect_err("expected refuse");
    assert_eq!(refusal.reason, "bash refuse");
}

// ---------------------------------------------------------------------------
// Custom — control case; substrate-internal memory_write gate
// ---------------------------------------------------------------------------

#[test]
fn custom_memory_write_routes_through_wire_check() {
    install_routing_hook();
    let allow = AgentAction::Custom {
        custom_kind: "memory_write".into(),
        payload: serde_json::json!({"namespace": "public/notes"}),
    };
    assert!(wire_check::check(&allow).is_ok());
    let refuse = AgentAction::Custom {
        custom_kind: "__refuse__-custom".into(),
        payload: serde_json::json!({}),
    };
    let refusal = wire_check::check(&refuse).expect_err("expected refuse");
    assert_eq!(refusal.reason, "custom refuse");
}

// ---------------------------------------------------------------------------
// Display + Error contract — every refusal carries operator-readable text
// ---------------------------------------------------------------------------

#[test]
fn refusal_display_contains_governance_marker() {
    install_routing_hook();
    let action = AgentAction::FilesystemWrite {
        path: "/__refuse__".into(),
        byte_estimate: None,
    };
    let refusal = wire_check::check(&action).expect_err("expected refuse");
    let s = format!("{refusal}");
    assert!(s.contains("governance-refused"), "got: {s}");
    assert!(s.contains("filesystem_write refuse"), "got: {s}");
}
