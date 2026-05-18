// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #863 regression — `ai-memory governance check-action` CLI
//! subcommand parity with the MCP tool `memory_check_agent_action`.
//!
//! Pins:
//!   1. `--kind filesystem_write --path /tmp/foo.txt` matches the
//!      seeded R001 hard rule and surfaces a `refuse` verdict with
//!      `rule_id = R001`.
//!   2. `--kind process_spawn --binary forbidden-bin` matches a
//!      hard-fired (no-disk-threshold) seed rule and surfaces a
//!      `refuse` verdict. R004 in production keys off
//!      `disk_free_min_gib` which depends on the host's free disk;
//!      this test installs an R004-shape rule WITHOUT the disk
//!      threshold so the refusal fires deterministically on any
//!      runner.
//!   3. `--kind filesystem_write --path $HOME/.local-runs/ok.txt`
//!      does NOT match R001 (glob is `/tmp/**` only) so the verdict
//!      is `allow`.
//!
//! The test drives the real built binary through `assert_cmd` so the
//! clap arg parser + dispatch arm + shared `run_check` core all
//! exercise end-to-end. Each test gets a fresh sqlite file seeded
//! with the exact rules it asserts on; no /tmp paths are CREATED
//! (the `/tmp/foo.txt` argument is a *string* fed to the rule
//! engine, not a touch).

#![allow(clippy::zombie_processes)]

use std::path::Path;

use assert_cmd::Command;
use rusqlite::params;
use tempfile::TempDir;

/// Build the standard `ai-memory --db <db>` invocation. Honors the
/// project no-config invariant so the test never picks up the
/// operator's real `config.toml`.
fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

/// Create a fresh sqlite DB with the minimum schema `check_agent_action`
/// needs (`governance_rules` + `signed_events`) and seed two rules:
///   * `R001` — `filesystem_write` glob `/tmp/**`, refuse.
///   * `R004F` — `process_spawn` binary `forbidden-bin`, refuse, NO
///     `disk_free_min_gib` so it fires deterministically.
fn seed_rules_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
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
             prev_hash BLOB,
             sequence INTEGER
         );",
    )
    .unwrap();
    for (id, kind, matcher, reason) in [
        (
            "R001",
            "filesystem_write",
            r#"{"glob":"/tmp/**"}"#,
            "Operator hard rule (#691): no /tmp writes.",
        ),
        (
            "R004F",
            "process_spawn",
            r#"{"binary":"forbidden-bin"}"#,
            "Operator hard rule: forbidden binary.",
        ),
    ] {
        conn.execute(
            "INSERT INTO governance_rules (id, kind, matcher, severity, reason, \
             namespace, created_by, created_at, enabled, signature, attest_level) \
             VALUES (?1, ?2, ?3, 'refuse', ?4, '_global', 'system:seed', 0, 1, NULL, 'unsigned')",
            params![id, kind, matcher, reason],
        )
        .unwrap();
    }
}

/// Suppress operator-pubkey resolution in the spawned child so the
/// unsigned seed rules above enforce regardless of dev-host /
/// CI-runner state (cf. issue #819).
///
/// `resolve_operator_pubkey` (`src/governance/rules_store.rs`) checks
/// two sources: (1) `AI_MEMORY_OPERATOR_PUBKEY` env var, and (2) the
/// `operator.key.pub` file under `dirs::config_dir()`. We point HOME
/// (and on Linux, `XDG_CONFIG_HOME`) at an empty tempdir so the second
/// source returns None; and we clear the env var so the first source
/// returns None. With both sources None, `enforced_rule_passes`
/// admits unsigned `enabled = 1` rows (pre-L1-6 posture), which is
/// the matrix this regression test exercises.
fn no_pubkey_env(cmd: &mut Command, fake_home: &Path) {
    cmd.env("HOME", fake_home.to_str().unwrap())
        .env(
            "XDG_CONFIG_HOME",
            fake_home.join(".config").to_str().unwrap(),
        )
        .env_remove("AI_MEMORY_OPERATOR_PUBKEY");
}

#[test]
fn cli_refuses_filesystem_write_to_tmp() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("rules.db");
    let fake_home = tmp.path().join("fake-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    seed_rules_db(&db);

    let mut cmd = ai_memory(&db);
    no_pubkey_env(&mut cmd, &fake_home);
    let out = cmd
        .args([
            "governance",
            "check-action",
            "--kind",
            "filesystem_write",
            "--path",
            "/tmp/foo.txt",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let raw = String::from_utf8(out).expect("utf-8 stdout");
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("expected JSON; got: {raw}\nerror: {e}"));
    assert_eq!(
        v["decision"]["decision"], "refuse",
        "expected refuse for /tmp/** path; got: {v}"
    );
    assert_eq!(
        v["decision"]["rule_id"], "R001",
        "expected R001 to fire; got: {v}"
    );
    assert_eq!(v["kind"], "filesystem_write");
}

#[test]
fn cli_refuses_process_spawn_on_binary_match() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("rules.db");
    let fake_home = tmp.path().join("fake-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    seed_rules_db(&db);

    let mut cmd = ai_memory(&db);
    no_pubkey_env(&mut cmd, &fake_home);
    let out = cmd
        .args([
            "governance",
            "check-action",
            "--kind",
            "process_spawn",
            "--binary",
            "forbidden-bin",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let raw = String::from_utf8(out).expect("utf-8 stdout");
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("expected JSON; got: {raw}\nerror: {e}"));
    assert_eq!(
        v["decision"]["decision"], "refuse",
        "expected refuse for forbidden-bin; got: {v}"
    );
    assert_eq!(
        v["decision"]["rule_id"], "R004F",
        "expected R004F to fire; got: {v}"
    );
}

#[test]
fn cli_allows_filesystem_write_under_local_runs() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("rules.db");
    let fake_home = tmp.path().join("fake-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    seed_rules_db(&db);

    // Use $HOME/.local-runs/ok.txt as the candidate path. The R001
    // glob is `/tmp/**` only, so this path does NOT match and the
    // verdict must be allow. We do NOT actually create the file —
    // the substrate rule engine works on the path string alone.
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/test".to_string());
    let path = format!("{home}/.local-runs/ok.txt");

    let mut cmd = ai_memory(&db);
    no_pubkey_env(&mut cmd, &fake_home);
    let out = cmd
        .args([
            "governance",
            "check-action",
            "--kind",
            "filesystem_write",
            "--path",
            &path,
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let raw = String::from_utf8(out).expect("utf-8 stdout");
    let v: serde_json::Value = serde_json::from_str(raw.trim())
        .unwrap_or_else(|e| panic!("expected JSON; got: {raw}\nerror: {e}"));
    assert_eq!(
        v["decision"]["decision"], "allow",
        "expected allow for {path}; got: {v}"
    );
}

#[test]
fn cli_human_output_prints_allow_verdict() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("rules.db");
    let fake_home = tmp.path().join("fake-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    seed_rules_db(&db);

    let mut cmd = ai_memory(&db);
    no_pubkey_env(&mut cmd, &fake_home);
    let out = cmd
        .args([
            "governance",
            "check-action",
            "--kind",
            "filesystem_write",
            "--path",
            "/home/user/safe.txt",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).expect("utf-8 stdout");
    assert_eq!(stdout.trim(), "Allow", "human verdict line: {stdout}");
}

#[test]
fn cli_human_output_prints_refuse_with_rule_id() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("rules.db");
    let fake_home = tmp.path().join("fake-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    seed_rules_db(&db);

    let mut cmd = ai_memory(&db);
    no_pubkey_env(&mut cmd, &fake_home);
    let out = cmd
        .args([
            "governance",
            "check-action",
            "--kind",
            "filesystem_write",
            "--path",
            "/tmp/some-file",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(out).expect("utf-8 stdout");
    assert!(
        stdout.contains("Refuse"),
        "expected Refuse verdict; got: {stdout}"
    );
    assert!(
        stdout.contains("R001"),
        "expected rule_id R001; got: {stdout}"
    );
}
