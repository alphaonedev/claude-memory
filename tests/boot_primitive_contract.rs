// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #487 PR-3 — universal `ai-memory boot` primitive contract.
//!
//! These tests spawn the actual `ai-memory` binary (via `assert_cmd`) and
//! pin the contract every session-boot integration recipe in
//! `docs/integrations/` depends on. If `boot.rs`'s output shape, status
//! header text, or exit-code semantics regress, the matching test fails on
//! the next nightly run and the regression has a name.
//!
//! Cross-platform: every path goes through `tempfile`, every assertion is
//! over text the binary writes (no `#[cfg(unix)]` gates). The `windows-latest`
//! CI runner exercises this same file unchanged.

use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;
use tempfile::TempDir;

/// Build the standard `ai-memory --db <tmp>` command shape with
/// `AI_MEMORY_NO_CONFIG=1` so the user's `~/.config/ai-memory/config.toml`
/// (which may set `tier=autonomous` and trigger embedder cold-load) is
/// bypassed in CI.
fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

/// Seed one memory directly via the `ai-memory store` subcommand. We use
/// the binary instead of `db::insert` so this test file stays a strict
/// black-box against the published CLI surface.
fn seed_memory(db: &Path, namespace: &str, title: &str, content: &str) {
    ai_memory(db)
        .args([
            "--json", "store", "-n", namespace, "-T", title, "-c", content,
        ])
        .assert()
        .success();
}

#[test]
fn boot_emits_ok_status_with_seeded_db() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    seed_memory(&db, "ns-ok", "first-memory", "content one");
    seed_memory(&db, "ns-ok", "second-memory", "content two");

    let assert = ai_memory(&db)
        .args(["boot", "--namespace", "ns-ok", "--limit", "10"])
        .assert()
        .success();
    let output = assert.get_output();
    let stdout = std::str::from_utf8(&output.stdout).unwrap();

    assert!(
        stdout.starts_with("# ai-memory boot: ok"),
        "expected ok header at start of stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("ns=ns-ok"),
        "header must echo namespace: {stdout}"
    );
    assert!(
        stdout.contains("first-memory"),
        "expected seeded memory in body: {stdout}"
    );
}

#[test]
fn boot_emits_warn_status_when_db_path_missing() {
    let tmp = TempDir::new().unwrap();
    // A path inside a directory that does not exist — db::open fails
    // because the parent dir is missing. Boot's contract: exit 0, surface
    // `# ai-memory boot: warn — db unavailable` so the agent sees it.
    let bad_db = tmp.path().join("nope/does-not-exist/db.sqlite");

    let assert = ai_memory(&bad_db)
        .args(["boot", "--quiet", "--namespace", "ns"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();

    assert!(
        stdout.starts_with("# ai-memory boot: warn"),
        "expected warn header, got: {stdout}"
    );
    assert!(
        stdout.contains("db unavailable"),
        "warn header must explain cause: {stdout}"
    );
}

#[test]
fn boot_emits_info_empty_status_for_empty_namespace() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    // Initialize the DB with a row in *some* namespace, but query an
    // unrelated namespace. Without any global Long-tier fallback, boot
    // should emit `info — namespace 'X' is empty`.
    seed_memory(&db, "ns-other", "elsewhere", "x");

    let assert = ai_memory(&db)
        .args(["boot", "--namespace", "nothing-here", "--limit", "5"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();

    assert!(
        stdout.starts_with("# ai-memory boot: info"),
        "expected info header, got: {stdout}"
    );
    assert!(
        stdout.contains("nothing-here") && stdout.contains("empty"),
        "info-empty header must name the namespace: {stdout}"
    );
}

#[test]
fn boot_emits_info_fallback_status_when_namespace_empty_but_global_long_present() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    seed_memory(&db, "global-ns", "long-tier-row", "x");
    // Promote to long via the CLI so the global fallback finds it.
    ai_memory(&db)
        .args(["--json", "list", "--namespace", "global-ns", "--limit", "1"])
        .assert()
        .success();
    // Use rusqlite directly to flip the tier — boot.rs's fallback path
    // queries `tier=Long` globally. We set tier=long on the seeded row so
    // the namespace miss + global Long fallback combination fires.
    let conn = ai_memory::db::open(&db).unwrap();
    conn.execute("UPDATE memories SET tier='long'", []).unwrap();
    drop(conn);

    let assert = ai_memory(&db)
        .args(["boot", "--namespace", "missing-ns", "--limit", "5"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();

    assert!(
        stdout.starts_with("# ai-memory boot: info"),
        "expected info header, got: {stdout}"
    );
    assert!(
        stdout.contains("fallback"),
        "info-fallback header must say 'fallback': {stdout}"
    );
    assert!(
        stdout.contains("long-tier-row"),
        "expected fallback body: {stdout}"
    );
}

#[test]
fn boot_json_format_status_is_machine_parseable() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    seed_memory(&db, "ns-json", "row-one", "x");

    let assert = ai_memory(&db)
        .args(["boot", "--namespace", "ns-json", "--format", "json"])
        .assert()
        .success();
    let stdout = std::str::from_utf8(&assert.get_output().stdout).unwrap();

    let parsed: Value = serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("boot --format json must produce valid JSON; err={e}; out={stdout}")
    });
    assert!(
        parsed.get("status").is_some(),
        "JSON output must carry a `status` field: {parsed}"
    );
    assert_eq!(parsed["status"], "ok");
    assert_eq!(parsed["namespace"], "ns-json");
    assert!(parsed["memories"].is_array(), "memories must be an array");
}

#[test]
fn boot_quiet_suppresses_stderr_only() {
    let tmp = TempDir::new().unwrap();
    let bad_db = tmp.path().join("does/not/exist/db.sqlite");

    let assert = ai_memory(&bad_db)
        .args(["boot", "--quiet"])
        .assert()
        .success();
    let out = assert.get_output();
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    let stderr = std::str::from_utf8(&out.stderr).unwrap();

    // --quiet alone: stderr silent, stdout still has the diagnostic header.
    assert!(
        stderr.is_empty(),
        "stderr must be empty under --quiet, got: {stderr}"
    );
    assert!(
        !stdout.is_empty() && stdout.contains("# ai-memory boot:"),
        "stdout must still carry the diagnostic header under --quiet: {stdout}"
    );
}

#[test]
fn boot_no_header_with_quiet_is_fully_silent() {
    let tmp = TempDir::new().unwrap();
    let bad_db = tmp.path().join("does/not/exist/db.sqlite");

    let assert = ai_memory(&bad_db)
        .args(["boot", "--quiet", "--no-header"])
        .assert()
        .success();
    let out = assert.get_output();

    assert!(
        out.stdout.is_empty(),
        "stdout must be empty under --quiet --no-header, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        out.stderr.is_empty(),
        "stderr must be empty under --quiet --no-header, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn boot_exit_code_is_zero_in_all_states() {
    let tmp = TempDir::new().unwrap();

    // 1) ok — DB present and namespace non-empty.
    let db_ok = tmp.path().join("ok.db");
    seed_memory(&db_ok, "ns-ok", "row", "x");
    ai_memory(&db_ok)
        .args(["boot", "--namespace", "ns-ok"])
        .assert()
        .success();

    // 2) info-empty — DB present but namespace empty, no global Long fallback.
    let db_empty = tmp.path().join("empty.db");
    seed_memory(&db_empty, "ns-some", "x", "x");
    ai_memory(&db_empty)
        .args(["boot", "--namespace", "missing-ns"])
        .assert()
        .success();

    // 3) warn — DB unreachable.
    let db_bad = tmp.path().join("nope/does-not-exist/db.sqlite");
    ai_memory(&db_bad)
        .args(["boot", "--quiet"])
        .assert()
        .success();
}
