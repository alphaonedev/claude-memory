// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for `ai-memory install --hook pretool` (v0.7.0
//! policy-engine item 2, issue #691).
//!
//! These tests drive the public CLI surface end-to-end:
//!
//! 1. `install_writes_hook_to_fresh_settings_json` — empty
//!    `~/.claude/settings.json` (we use a tempdir) → install runs →
//!    file contains the documented `PreToolUse` entry.
//! 2. `install_preserves_existing_keys` — pre-existing `permissions`,
//!    `env`, etc. survive the install.
//! 3. `install_appends_to_existing_pretooluse_array` — operator's
//!    existing entry is preserved, our entry is appended.
//! 4. `install_refuses_overwrite_without_force` — a conflicting hook
//!    config triggers a clean exit with a `--force` hint.
//! 5. `install_with_force_overwrites` — same as #4 with `--force`
//!    succeeds.
//! 6. `installed_hook_smoke_test_invokes_check_action` — drives
//!    `memory_check_agent_action` directly with a synthesised
//!    `PreToolUse` payload (Bash `rm -rf /tmp/foo`) against a fresh DB;
//!    proves Allow when no rule, Refuse when R001-shaped rule
//!    enabled.
//!
//! Each test uses `tempfile::tempdir()` and `--config
//! <tempdir>/settings.json` so the real `~/.claude/settings.json`
//! never gets touched.

use std::process::Command as StdCommand;

use assert_cmd::Command;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Returns an `assert_cmd::Command` for `ai-memory install claude-code`
/// with `AI_MEMORY_NO_CONFIG=1` so tests don't accidentally load the
/// host's `~/.config/ai-memory/config.toml`.
fn install_cmd() -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["install", "claude-code"]);
    cmd
}

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn read_json(path: &std::path::Path) -> Value {
    let s = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&s).unwrap()
}

#[test]
fn install_writes_hook_to_fresh_settings_json() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(&cfg, "{}\n");

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("installed PreToolUse hook ->"));

    let parsed = read_json(&cfg);
    let arr = parsed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    let entry = &arr[0];
    assert_eq!(entry["matcher"], "*");
    assert_eq!(entry["hooks"][0]["type"], "mcp_tool");
    assert_eq!(entry["hooks"][0]["tool"], "memory_check_agent_action");
}

#[test]
fn install_preserves_existing_keys() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(
        &cfg,
        r#"{"permissions":{"allow":["npm:*"]},"env":{"FOO":"bar"},"theme":"dark"}"#,
    );

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .success();

    let parsed = read_json(&cfg);
    assert_eq!(parsed["permissions"]["allow"][0], "npm:*");
    assert_eq!(parsed["env"]["FOO"], "bar");
    assert_eq!(parsed["theme"], "dark");
    assert!(parsed["hooks"]["PreToolUse"].is_array());
}

#[test]
fn install_appends_to_existing_pretooluse_array() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(
        &cfg,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}"#,
    );

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .success();

    let parsed = read_json(&cfg);
    let arr = parsed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 2, "operator entry + our managed entry");
    // Operator's entry preserved at the original position.
    assert_eq!(arr[0]["matcher"], "Bash");
    assert_eq!(arr[0]["hooks"][0]["command"], "echo hi");
    // Ours appended at the end.
    assert_eq!(arr[1]["matcher"], "*");
    assert_eq!(arr[1]["hooks"][0]["tool"], "memory_check_agent_action");
}

#[test]
fn install_refuses_overwrite_without_force() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    // Conflicting hook config: also names memory_check_agent_action but
    // with a different (non-`*`) matcher. The operator scoped the hook
    // intentionally — clobbering would silently change their policy.
    write(
        &cfg,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"mcp_tool","tool":"memory_check_agent_action"}]}]}}"#,
    );

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--force"));

    // File must NOT have been modified.
    let parsed = read_json(&cfg);
    let arr = parsed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["matcher"], "Bash");
}

#[test]
fn install_with_force_overwrites() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(
        &cfg,
        r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"mcp_tool","tool":"memory_check_agent_action"}]}]}}"#,
    );

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
            "--force",
        ])
        .assert()
        .success();

    let parsed = read_json(&cfg);
    let arr = parsed["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(arr.len(), 1, "conflicting entry replaced with ours");
    assert_eq!(arr[0]["matcher"], "*");
    assert_eq!(arr[0]["hooks"][0]["tool"], "memory_check_agent_action");
}

#[test]
fn install_dry_run_default_does_not_write() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(&cfg, "{}\n");
    let mtime_before = std::fs::metadata(&cfg).unwrap().modified().unwrap();

    install_cmd()
        .args(["--config", cfg.to_str().unwrap(), "--hook", "pretool"])
        .assert()
        .success()
        .stdout(predicates::str::contains("dry-run"));

    let mtime_after = std::fs::metadata(&cfg).unwrap().modified().unwrap();
    assert_eq!(mtime_before, mtime_after, "dry-run must not write");
}

#[test]
fn install_uninstall_round_trips() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    let original = r#"{"theme":"dark"}"#;
    write(&cfg, original);

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .success();

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--uninstall",
            "--apply",
        ])
        .assert()
        .success();

    let parsed = read_json(&cfg);
    assert_eq!(parsed["theme"], "dark");
    assert!(
        parsed.get("hooks").is_none(),
        "PreToolUse gone after uninstall"
    );
}

#[test]
fn install_rejects_hook_flag_on_non_claude_code() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("mcp.json");
    write(&cfg, "{}\n");

    // Override the subcommand to `cursor`. We can't use `install_cmd()`
    // because it pins `claude-code`.
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1");
    cmd.args([
        "install",
        "cursor",
        "--config",
        cfg.to_str().unwrap(),
        "--hook",
        "pretool",
        "--apply",
    ]);
    cmd.assert().failure().stderr(predicates::str::contains(
        "only supported for `claude-code`",
    ));
}

/// Smoke test: a synthesised `PreToolUse` payload (Bash command
/// attempting `rm -rf /tmp/foo`) routed through the actual
/// `memory_check_agent_action` MCP tool returns `allow` when no rule
/// is enabled, and `refuse` after the R001-shaped rule is seeded +
/// enabled. This is the end-to-end proof that what the installer
/// wires up is what gets enforced.
///
/// The test uses the in-process `handle_check_agent_action` entry
/// point rather than spawning a full MCP server, because that's the
/// same code path the JSON-RPC dispatch lands on after argument
/// parsing.
#[test]
#[allow(clippy::too_many_lines)]
fn installed_hook_smoke_test_invokes_check_action() {
    use std::sync::Mutex;

    use ai_memory::governance::rules_store::{self, Rule};
    use ai_memory::mcp::handle_check_agent_action;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use rand_core::OsRng;

    // Module-scope mutex so any future test in this crate that also
    // mutates `AI_MEMORY_OPERATOR_PUBKEY` can share it. Static so it
    // survives across `cargo test`'s parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    // Minimal schema for governance_rules + signed_events (mirrors
    // the migration that production runs).
    conn.execute_batch(
        "CREATE TABLE governance_rules (
             id TEXT PRIMARY KEY,
             kind TEXT NOT NULL,
             matcher TEXT NOT NULL,
             severity TEXT NOT NULL,
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

    // Synthesise the Claude Code PreToolUse payload shape (Bash tool
    // proposing `rm -rf /tmp/foo`) and translate to the
    // memory_check_agent_action call shape.
    let bash_payload = json!({
        "kind": "bash",
        "command": "rm -rf /tmp/foo",
        "cwd": "/Users/operator/proj",
        "agent_id": "ai:claude-code@host:pid-1234",
    });

    // Phase 1: no rule installed → Allow.
    let r = handle_check_agent_action(&conn, &bash_payload).unwrap();
    assert_eq!(
        r["decision"]["decision"], "allow",
        "with no rule enabled, the substrate engine permits the action"
    );

    // Phase 2: install the R001-shaped rule that refuses /tmp writes,
    // but applied to the bash kind so the rm dispatch is caught.
    // For `bash` kind the matcher uses `command_regex` (treated as a
    // literal substring, see `match_bash` in agent_action.rs).
    //
    // L1-6 hermeticity: when the host has an operator pubkey on disk
    // (`~/Library/Application Support/ai-memory/operator.key.pub`) or
    // sets `AI_MEMORY_OPERATOR_PUBKEY`, `enforced_rule_passes` skips
    // any rule whose `attest_level != "operator_signed"`. To make this
    // test pass deterministically regardless of host state, we
    // generate a one-off test keypair, point the env var at its
    // pubkey under a mutex (so parallel tests in this crate don't
    // race), and sign the rule with the corresponding signing key
    // before insert. The whole pubkey-resolution path is then
    // satisfied by the in-test key, not the host's.
    let signing = SigningKey::generate(&mut OsRng);
    let pub_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    let env_guard = ENV_LOCK.lock().unwrap();
    let prev_pubkey = std::env::var("AI_MEMORY_OPERATOR_PUBKEY").ok();
    // SAFETY: serialized across tests in this crate by `ENV_LOCK`;
    // restored at the end of the test scope.
    unsafe {
        std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", &pub_b64);
    }

    let mut rule = Rule {
        id: "R001-test".into(),
        kind: "bash".into(),
        matcher: r#"{"command_regex":"rm -rf /tmp"}"#.into(),
        severity: "refuse".into(),
        reason: "no /tmp destruction".into(),
        namespace: "_global".into(),
        created_by: "test-operator".into(),
        created_at: 0,
        enabled: true,
        signature: None,
        attest_level: "operator_signed".into(),
    };
    let canonical = rules_store::canonical_bytes_for_signing(&rule).unwrap();
    rule.signature = Some(signing.sign(&canonical).to_bytes().to_vec());
    rules_store::insert(&conn, &rule).unwrap();

    let r = handle_check_agent_action(&conn, &bash_payload).unwrap();

    // Restore prior env var before the assertion so a failure leaves
    // the process env clean for the next test slotted on this thread.
    // SAFETY: serialized by `_env_guard`.
    unsafe {
        match prev_pubkey {
            Some(v) => std::env::set_var("AI_MEMORY_OPERATOR_PUBKEY", v),
            None => std::env::remove_var("AI_MEMORY_OPERATOR_PUBKEY"),
        }
    }
    drop(env_guard);

    assert_eq!(
        r["decision"]["decision"], "refuse",
        "after the rule is enabled, the same payload is refused"
    );
    assert_eq!(r["decision"]["rule_id"], "R001-test");
    // Audit chain row was emitted (one for each call).
    let row_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM signed_events", [], |r| r.get(0))
        .unwrap();
    assert!(
        row_count >= 2,
        "expected at least two governance.check audit rows, got {row_count}"
    );
}

#[test]
fn install_help_mentions_hook_flag() {
    // Make sure clap renders `--hook` in `ai-memory install claude-code --help`.
    Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args(["install", "claude-code", "--help"])
        .assert()
        .success()
        .stdout(predicates::str::contains("--hook"));
}

/// End-to-end install + binary spawn check: after `--apply`, running
/// `ai-memory --help` still works (the installer didn't accidentally
/// corrupt the binary's runtime environment). Smoke-level — just
/// proves the binary remains operable.
#[test]
fn binary_remains_operable_after_install() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    write(&cfg, "{}\n");

    install_cmd()
        .args([
            "--config",
            cfg.to_str().unwrap(),
            "--hook",
            "pretool",
            "--apply",
        ])
        .assert()
        .success();

    let out = StdCommand::new(env!("CARGO_BIN_EXE_ai-memory"))
        .env("AI_MEMORY_NO_CONFIG", "1")
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
}
