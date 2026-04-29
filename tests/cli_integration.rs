// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Wave 7 / I7 — CLI surface regression guards.
//!
//! These tests spawn the real `ai-memory` binary (via `assert_cmd`) and
//! exercise the clap-derived CLI surface end-to-end. They don't add
//! coverage of new code — they are smoke tests that fail loudly if the
//! binary's CLI parses, exit codes, or JSON output shapes regress.
//!
//! All tests set `AI_MEMORY_NO_CONFIG=1` so the user's
//! `~/.config/ai-memory/config.toml` (which may set `tier=autonomous`,
//! triggering embedder/LLM init) is bypassed. Each test gets a unique
//! tempdir for the DB so the suite parallelises cleanly.

use std::io::Write;
use std::process::{Command as StdCommand, Stdio};

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Build the standard `ai-memory --db <tmp>` command shape with
/// `AI_MEMORY_NO_CONFIG=1` set. The returned `Command` still needs the
/// subcommand and any extra args.
fn ai_memory(db: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

/// Build a `std::process::Command` for the binary so callers can pipe
/// to/from stdin / stdout (assert_cmd::Command doesn't surface those).
fn ai_memory_std(db: &std::path::Path) -> StdCommand {
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_ai-memory"));
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

#[test]
fn binary_help_succeeds() {
    Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI-agnostic persistent memory"));
}

#[test]
fn binary_version_succeeds() {
    Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("ai-memory"));
}

#[test]
fn each_subcommand_help() {
    // Parametrised: every subcommand from `Cli::Command` must accept
    // `--help` and exit 0. If a future subcommand is added without a
    // help body this test catches it.
    let subcommands = vec![
        "serve",
        "mcp",
        "store",
        "update",
        "recall",
        "search",
        "get",
        "list",
        "delete",
        "promote",
        "forget",
        "link",
        "consolidate",
        "gc",
        "stats",
        "namespaces",
        "export",
        "import",
        "resolve",
        "shell",
        "sync",
        "sync-daemon",
        "auto-consolidate",
        "completions",
        "man",
        "mine",
        "archive",
        "agents",
        "pending",
        "backup",
        "restore",
        "curator",
        "bench",
    ];
    for sub in subcommands {
        Command::cargo_bin("ai-memory")
            .unwrap()
            .env("AI_MEMORY_NO_CONFIG", "1")
            .args([sub, "--help"])
            .assert()
            .success();
    }
}

#[test]
fn store_then_get_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    let out = ai_memory(&db)
        .args([
            "--json",
            "store",
            "-T",
            "roundtrip-title",
            "-c",
            "roundtrip-content",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    ai_memory(&db)
        .args(["get", &id])
        .assert()
        .success()
        .stdout(predicate::str::contains("roundtrip-title"));
}

#[test]
fn store_then_recall() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    ai_memory(&db)
        .args([
            "--json",
            "store",
            "-T",
            "kotlin-notes",
            "-c",
            "kotlin coroutines unique keyword",
        ])
        .assert()
        .success();
    // Pin to keyword tier to skip embedder init (semantic loads MiniLM,
    // which is multi-second cold-start per test).
    ai_memory(&db)
        .args(["--json", "recall", "kotlin", "--tier", "keyword"])
        .assert()
        .success()
        .stdout(predicate::str::contains("kotlin"));
}

#[test]
fn store_then_list() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    for i in 0..3 {
        ai_memory(&db)
            .args([
                "--json",
                "store",
                "-T",
                &format!("title-{i}"),
                "-c",
                &format!("content-{i}"),
            ])
            .assert()
            .success();
    }
    let out = ai_memory(&db)
        .args(["--json", "list", "--limit", "100"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // The list output has shape {"count": N, "memories": [...]}
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let count = v["memories"].as_array().map(Vec::len).unwrap_or(0);
    assert!(count >= 3, "expected >=3 memories, got {count}");
}

#[test]
fn store_then_search() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    ai_memory(&db)
        .args([
            "--json",
            "store",
            "-T",
            "search-needle",
            "-c",
            "uniqueneedleword in the haystack",
        ])
        .assert()
        .success();
    let out = ai_memory(&db)
        .args(["--json", "search", "uniqueneedleword"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["count"].as_u64().unwrap_or(0) >= 1,
        "expected search hit, got: {v:?}"
    );
}

#[test]
fn store_then_delete() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    let out = ai_memory(&db)
        .args(["--json", "store", "-T", "delme", "-c", "delme-body"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let id = v["id"].as_str().unwrap().to_string();

    ai_memory(&db).args(["delete", &id]).assert().success();

    // After delete, get returns exit 1
    ai_memory(&db).args(["get", &id]).assert().failure();
}

#[test]
fn store_with_stdin_content() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    let mut child = ai_memory_std(&db)
        .args(["--json", "store", "-T", "stdin-test", "-c", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"hello-from-stdin").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["content"]
            .as_str()
            .unwrap_or("")
            .contains("hello-from-stdin"),
        "got: {v:?}"
    );
}

#[test]
fn export_import_roundtrip() {
    let tmp = TempDir::new().unwrap();
    let db1 = tmp.path().join("source.db");
    let db2 = tmp.path().join("dest.db");
    let export_path = tmp.path().join("export.json");

    for i in 0..5 {
        ai_memory(&db1)
            .args([
                "--json",
                "store",
                "-T",
                &format!("export-{i}"),
                "-c",
                &format!("body-{i}"),
            ])
            .assert()
            .success();
    }
    let out = ai_memory(&db1)
        .args(["--json", "export"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    std::fs::write(&export_path, &out).unwrap();

    // Import via stdin into the fresh DB
    let child = ai_memory_std(&db2)
        .args(["--json", "import"])
        .stdin(Stdio::from(std::fs::File::open(&export_path).unwrap()))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "import failed: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );

    let list_out = ai_memory(&db2)
        .args(["--json", "list", "--limit", "100"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&list_out).unwrap();
    let count = v["memories"].as_array().map(Vec::len).unwrap_or(0);
    assert_eq!(count, 5, "expected 5 imported, got {count}");
}

#[test]
fn stats_empty_db() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("empty.db");
    let out = ai_memory(&db)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["total"].as_u64().unwrap_or(99), 0, "got: {v:?}");
}

#[test]
fn stats_with_data() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("filled.db");
    for i in 0..2 {
        ai_memory(&db)
            .args(["--json", "store", "-T", &format!("s-{i}"), "-c", "body"])
            .assert()
            .success();
    }
    let out = ai_memory(&db)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(
        v["total"].as_u64().unwrap_or(0) >= 2,
        "expected >=2 total, got: {v:?}"
    );
}

#[test]
fn namespaces_command() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ns.db");
    ai_memory(&db)
        .args([
            "--json",
            "store",
            "-T",
            "x",
            "-c",
            "y",
            "-n",
            "test-namespace-i7",
        ])
        .assert()
        .success();
    let out = ai_memory(&db)
        .args(["--json", "namespaces"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8_lossy(&out);
    assert!(
        s.contains("test-namespace-i7"),
        "expected namespace in output: {s}"
    );
}

#[test]
fn forget_by_namespace() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("forget.db");
    ai_memory(&db)
        .args(["--json", "store", "-T", "f1", "-c", "y", "-n", "doomed"])
        .assert()
        .success();
    ai_memory(&db)
        .args(["--json", "store", "-T", "f2", "-c", "y", "-n", "doomed"])
        .assert()
        .success();

    ai_memory(&db)
        .args(["--json", "forget", "-n", "doomed"])
        .assert()
        .success();

    let list_out = ai_memory(&db)
        .args(["--json", "list", "-n", "doomed", "--limit", "100"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&list_out).unwrap();
    let count = v["memories"].as_array().map(Vec::len).unwrap_or(99);
    assert_eq!(count, 0, "expected 0 after forget, got {count}: {v:?}");
}

#[test]
fn link_two_memories_then_get_links() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("link.db");
    let id1 = {
        let out = ai_memory(&db)
            .args(["--json", "store", "-T", "alpha", "-c", "a"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        v["id"].as_str().unwrap().to_string()
    };
    let id2 = {
        let out = ai_memory(&db)
            .args(["--json", "store", "-T", "beta", "-c", "b"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        v["id"].as_str().unwrap().to_string()
    };

    ai_memory(&db)
        .args(["--json", "link", &id1, &id2, "-r", "related_to"])
        .assert()
        .success();

    // `get` prints links underneath the memory body
    ai_memory(&db)
        .args(["get", &id1])
        .assert()
        .success()
        .stdout(predicate::str::contains("related_to"));
}

#[test]
fn consolidate_three_into_one() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("consol.db");
    let mut ids = Vec::new();
    for i in 0..3 {
        let out = ai_memory(&db)
            .args([
                "--json",
                "store",
                "-T",
                &format!("c{i}"),
                "-c",
                &format!("content-{i}"),
                "-n",
                "consol-ns",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        ids.push(v["id"].as_str().unwrap().to_string());
    }
    let csv = ids.join(",");
    ai_memory(&db)
        .args([
            "--json",
            "consolidate",
            &csv,
            "-T",
            "merged-title",
            "-s",
            "merged-summary",
            "-n",
            "consol-ns",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"id\""));
}

#[test]
fn shell_quit_immediately() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("shell.db");
    let mut child = ai_memory_std(&db)
        .arg("shell")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"quit\n").unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "shell exited non-zero: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn invalid_subcommand_errors_with_useful_message() {
    Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .arg("notarealcommand")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand"));
}

#[test]
fn missing_required_arg_errors() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("missing.db");
    ai_memory(&db)
        .arg("store")
        .assert()
        .failure()
        .stderr(predicate::str::contains("required"));
}

#[test]
fn invalid_tier_errors_with_validation_message() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("badtier.db");
    ai_memory(&db)
        .args(["store", "-T", "x", "-c", "y", "--tier", "wat"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid tier"));
}
