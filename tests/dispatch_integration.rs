// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! PR-9g (issue #487 coverage uplift) — `daemon_runtime::run` dispatch
//! integration tests.
//!
//! Audit A flagged `src/daemon_runtime.rs` at 70.45% line coverage after
//! issue #487's eight PRs landed. The `Command::*` arms added by every
//! #487 PR can't be exercised by `--lib` tests because dispatch is
//! fundamentally an integration concern — it needs the full binary spawn
//! plus argv parse plus side-effect propagation (process exit codes,
//! stdout/stderr framing, file system writes).
//!
//! These tests spawn `target/debug/ai-memory` via
//! [`assert_cmd::Command::cargo_bin`] and exercise the new and modified
//! Command variants from issue #487:
//!
//! - `Boot` (PR-3 + PR-4 manifest expansion)
//! - `Install` (PR-2 / PR-8) — claude-code target with dry-run, apply,
//!   uninstall, and the malformed-config refusal path.
//! - `Wrap` (PR-6) — with `echo` as the wrapped agent so the test is
//!   hermetic.
//! - `Logs` (PR-5) — tail against a pre-seeded log directory.
//! - `Audit` (PR-5) — verify against a real, hash-chained log emitted
//!   through the lib's public `audit::init` + `audit::emit` API; then
//!   tamper one line and assert the verify exits non-zero.
//! - `Doctor` (P7 / unchanged by #487 but routed through the same
//!   dispatch shell) — `--json` mode.
//!
//! We additionally cover the global flag plumbing on `Cli`
//! (`--db`, `--json`, `--agent-id`, `--db-passphrase-file`) and the
//! help / unknown-command paths so the dispatch outer skeleton is
//! exhaustively touched.
//!
//! All tests:
//! - Use `tempfile::TempDir` for paths so the host filesystem stays
//!   clean and runs are parallel-safe.
//! - Set `AI_MEMORY_NO_CONFIG=1` so the user's config never loads
//!   (avoids embedder/LLM init in `tier=autonomous` setups).
//! - Are deterministic — no time- or network-dependent assertions.

#![allow(clippy::too_many_lines)]

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Build the standard `ai-memory --db <tmp>` command shape. Every test
/// goes through this so `AI_MEMORY_NO_CONFIG=1` is set uniformly and
/// the global `--db` flag is wired before the subcommand argv. The
/// returned `Command` still needs the subcommand and any extra args.
fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

/// Same shape but without a `--db` flag for tests where the DB is not
/// load-bearing (install, help, unknown-command).
fn ai_memory_bare() -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1");
    cmd
}

// ---------------------------------------------------------------------------
// Boot dispatch (#487 PR-3 + PR-4)
// ---------------------------------------------------------------------------

#[test]
fn boot_dispatch_emits_header() {
    // Default text format — the universal hook contract is that stdout
    // begins with the `# ai-memory boot:` line.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("boot.db");
    let out = ai_memory(&db)
        .args(["boot", "--namespace", "audit-test", "--limit", "5"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.starts_with("# ai-memory boot:"),
        "expected header prefix, got first 80 chars: {:?}",
        &text.chars().take(80).collect::<String>()
    );
}

#[test]
fn boot_dispatch_no_header_suppresses_prefix() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("boot.db");
    let out = ai_memory(&db)
        .args([
            "boot",
            "--namespace",
            "audit-test",
            "--limit",
            "5",
            "--no-header",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        !text.contains("# ai-memory boot:"),
        "--no-header should suppress the header line; got: {text:?}"
    );
}

#[test]
fn boot_dispatch_quiet_exits_zero_on_missing_db() {
    // --quiet is the load-bearing flag for the SessionStart hook
    // contract: a missing DB must NOT block the agent's first turn.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("definitely-does-not-exist.db");
    ai_memory(&db)
        .args(["boot", "--namespace", "audit-test", "--quiet"])
        .assert()
        .success();
}

#[test]
fn boot_dispatch_format_json_emits_valid_json() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("boot.db");
    let out = ai_memory(&db)
        .args([
            "boot",
            "--namespace",
            "audit-test",
            "--limit",
            "5",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // Parses round-trip — fails the test if dispatch ever drops to text mode.
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|e| {
        panic!(
            "boot --format json must emit JSON; got {} ({e})",
            String::from_utf8_lossy(&out)
        )
    });
    assert!(v.get("namespace").is_some(), "missing namespace key: {v}");
    assert!(v.get("status").is_some(), "missing status key: {v}");
    assert!(v.get("count").is_some(), "missing count key: {v}");
}

// ---------------------------------------------------------------------------
// Install dispatch (#487 PR-2 / PR-8)
// ---------------------------------------------------------------------------

#[test]
fn install_dispatch_dry_run_prints_diff_and_does_not_write() {
    // The default contract: `ai-memory install <agent>` (no --apply) is
    // a dry-run. It must print a diff and leave the target file alone.
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");
    // No file at `cfg` — dry-run still computes `before == {}` → diff.
    ai_memory_bare()
        .args(["install", "claude-code", "--config", cfg.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("dry-run"))
        .stdout(predicate::str::contains("ai-memory:managed-block:start"));
    assert!(
        !cfg.exists(),
        "dry-run must NOT create the config file (got: {cfg:?})"
    );
}

#[test]
fn install_dispatch_apply_writes_file_then_uninstall_removes_block() {
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("settings.json");

    // 1. --apply: file gets created with the managed block.
    ai_memory_bare()
        .args([
            "install",
            "claude-code",
            "--config",
            cfg.to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .success();
    assert!(cfg.exists(), "--apply must create the config file");
    let body = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        body.contains("ai-memory:managed-block:start"),
        "managed block missing after apply: {body}"
    );

    // 2. --uninstall --apply: managed block gets surgically removed.
    ai_memory_bare()
        .args([
            "install",
            "claude-code",
            "--config",
            cfg.to_str().unwrap(),
            "--uninstall",
            "--apply",
        ])
        .assert()
        .success();
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        !after.contains("ai-memory:managed-block:start"),
        "uninstall --apply must remove the managed block; got: {after}"
    );
}

#[test]
fn install_dispatch_refuses_malformed_config() {
    // Malformed-config refusal path: never overwrite a config the user
    // might have made a typo in. Exit non-zero with a clear message.
    let tmp = TempDir::new().unwrap();
    let cfg = tmp.path().join("malformed.json");
    std::fs::write(&cfg, b"{not valid json").unwrap();
    ai_memory_bare()
        .args([
            "install",
            "claude-code",
            "--config",
            cfg.to_str().unwrap(),
            "--apply",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not valid JSON"));
    // The malformed file must be left alone, not overwritten.
    let after = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(after, "{not valid json");
}

// ---------------------------------------------------------------------------
// Wrap dispatch (#487 PR-6)
// ---------------------------------------------------------------------------

#[test]
fn wrap_dispatch_invokes_wrapped_agent_with_no_boot() {
    // `--no-boot` skips the inner boot call entirely so the test is
    // hermetic (no embedder, no DB read). `echo` is the wrapped agent —
    // it prints its argv to stdout, which lets us assert that the
    // system message and the trailing arg both reach the child.
    if cfg!(windows) {
        // Windows `echo` is a cmd.exe builtin and quotes argv
        // differently; the test is gated to Unix-like platforms where
        // `echo` is a real binary on $PATH. The dispatch arm is the
        // same on every platform — only the argv-passthrough assertion
        // is platform-specific.
        return;
    }
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("wrap.db");
    let out = ai_memory(&db)
        .args(["wrap", "echo", "--no-boot", "--", "hello-from-wrap"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("hello-from-wrap"),
        "trailing arg must reach the wrapped agent; got: {text}"
    );
    // Default strategy for an unknown agent is `--system <msg>`. echo
    // dumps that flag verbatim.
    assert!(
        text.contains("--system"),
        "default --system flag must be passed to the wrapped agent; got: {text}"
    );
    assert!(
        text.contains("ai-memory"),
        "the WRAP_PREAMBLE mentions ai-memory and must reach the agent; got: {text}"
    );
}

#[test]
fn wrap_dispatch_with_boot_includes_preamble() {
    // Without --no-boot, wrap calls the inner boot in-process. With a
    // tempdir DB the boot runs in --quiet mode internally so a missing
    // DB is OK; the assembled system message still carries the
    // preamble. Same Unix-only gate as above.
    if cfg!(windows) {
        return;
    }
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("wrap-boot.db");
    let out = ai_memory(&db)
        .args(["wrap", "echo", "--", "world"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("world"),
        "trailing arg must reach the wrapped agent; got: {text}"
    );
    assert!(
        text.contains("persistent memory"),
        "WRAP_PREAMBLE substring must reach the agent; got: {text}"
    );
}

// ---------------------------------------------------------------------------
// Logs dispatch (#487 PR-5)
// ---------------------------------------------------------------------------

#[test]
fn logs_dispatch_tail_against_seeded_dir() {
    // Seed an operational-log directory with a single line, then run
    // `logs tail --since <past-ts>` and assert the line round-trips.
    let tmp = TempDir::new().unwrap();
    let log_dir = tmp.path().join("logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    let log_file = log_dir.join("ai-memory.log");
    std::fs::write(
        &log_file,
        b"{\"timestamp\":\"2026-04-30T10:00:00Z\",\"level\":\"INFO\",\"msg\":\"dispatch-integration-test-marker\"}\n",
    )
    .unwrap();
    let out = ai_memory_bare()
        .args([
            "logs",
            "tail",
            "--since",
            "2025-01-01T00:00:00Z",
            "--log-dir",
            log_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("dispatch-integration-test-marker"),
        "logs tail must surface the seeded line; got: {text}"
    );
}

#[test]
fn logs_dispatch_tail_against_empty_dir_is_noop() {
    // Empty directory → exit 0, empty stdout. The CLI is a no-op on a
    // fresh install, never a hard error.
    let tmp = TempDir::new().unwrap();
    let log_dir = tmp.path().join("empty-logs");
    std::fs::create_dir_all(&log_dir).unwrap();
    ai_memory_bare()
        .args([
            "logs",
            "tail",
            "--since",
            "2025-01-01T00:00:00Z",
            "--log-dir",
            log_dir.to_str().unwrap(),
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Audit dispatch (#487 PR-5)
// ---------------------------------------------------------------------------

/// Process-wide lock that serializes audit-seeding tests. The audit
/// subsystem uses a singleton `SINK` + `SEQUENCE` pair (see
/// `src/audit.rs`) — two tests calling `audit::init` in parallel
/// would race and produce a partial chain. We hold this `Mutex`
/// for the duration of seed + verify in both audit chain tests so
/// the chain we hand to the spawned binary is well-formed.
static AUDIT_INIT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Build a known-good audit log file in `dir` by driving the lib's
/// public emission API. The audit subsystem is a process-wide
/// singleton; we install a sink at `dir/audit.log`, emit two events,
/// then return — the next call replaces our sink, which is fine
/// because we've already flushed the chain to disk and the spawned
/// `ai-memory audit verify` reads from the file, not the sink.
///
/// Returns the absolute path the chain was written to (we hand it to
/// the spawned binary via `--audit-dir`).
///
/// Caller MUST hold [`AUDIT_INIT_LOCK`] across seed + verify so two
/// parallel audit tests don't race on the singleton.
fn seed_audit_chain(dir: &Path) -> std::path::PathBuf {
    use ai_memory::audit::{AuditAction, EventBuilder, actor, emit, init, target_sweep};
    std::fs::create_dir_all(dir).unwrap();
    let log = dir.join("audit.log");
    init(&log, true, false).expect("audit::init should succeed against a tempdir");
    emit(EventBuilder::new(
        AuditAction::SessionBoot,
        actor("test-agent", "explicit_or_default", None),
        target_sweep("dispatch-integration"),
    ));
    emit(EventBuilder::new(
        AuditAction::SessionBoot,
        actor("test-agent", "explicit_or_default", None),
        target_sweep("dispatch-integration"),
    ));
    // Sanity check: confirm the chain has both events on disk before
    // we hand the path to the verifier. If the singleton was raced by
    // another test we'd see fewer lines here.
    let body = std::fs::read_to_string(&log).unwrap();
    let line_count = body.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(
        line_count >= 2,
        "seed_audit_chain wrote {line_count} lines (expected 2); audit singleton race?"
    );
    log
}

#[test]
fn audit_dispatch_verify_passes_on_known_good_chain() {
    let _guard = AUDIT_INIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = TempDir::new().unwrap();
    let audit_dir = tmp.path().join("audit");
    let _log = seed_audit_chain(&audit_dir);
    let out = ai_memory_bare()
        .args([
            "audit",
            "verify",
            "--audit-dir",
            audit_dir.to_str().unwrap(),
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("OK"),
        "verify must announce OK on a clean chain; got: {text}"
    );
}

#[test]
fn audit_dispatch_verify_fails_on_tampered_chain() {
    let _guard = AUDIT_INIT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let tmp = TempDir::new().unwrap();
    let audit_dir = tmp.path().join("audit");
    let log = seed_audit_chain(&audit_dir);

    // Tamper: rewrite line 2's `target.namespace`. The chain hash on
    // line 2 was computed over the prior content, so any mutation of
    // the namespace breaks `self_hash`. (The verifier checks
    // `prev_hash` and recomputes `self_hash`; either failure mode is
    // an acceptable signal of tampering. We pick a mutation that
    // changes content but keeps the line valid JSON, so the verifier
    // fails on hash mismatch rather than the parse path.)
    let raw = std::fs::read_to_string(&log).unwrap();
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    assert!(lines.len() >= 2, "seed should produce >=2 lines");
    lines[1] = lines[1].replace("dispatch-integration", "tampered-namespace");
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();

    // Verify must exit non-zero (per cli::audit, exit code 2 when the
    // chain breaks). assert_cmd's `failure()` accepts any non-zero.
    ai_memory_bare()
        .args([
            "audit",
            "verify",
            "--audit-dir",
            audit_dir.to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn audit_dispatch_verify_missing_log_is_noop() {
    // No file present → exit 0 with a friendly note. The audit CLI is
    // safe to run on a fresh install where audit was never enabled.
    let tmp = TempDir::new().unwrap();
    let audit_dir = tmp.path().join("audit-missing");
    std::fs::create_dir_all(&audit_dir).unwrap();
    ai_memory_bare()
        .args([
            "audit",
            "verify",
            "--audit-dir",
            audit_dir.to_str().unwrap(),
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Doctor dispatch (#487 routing of P7 doctor through daemon_runtime::run)
// ---------------------------------------------------------------------------

#[test]
fn doctor_dispatch_json_emits_valid_json() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("doctor.db");
    let assert_out = ai_memory(&db).args(["doctor", "--json"]).assert();
    let output = assert_out.get_output();
    // Doctor exit code: 0 on healthy, 2 on critical. assert_cmd's
    // `success()` requires 0; the doctor against a fresh empty DB has
    // historically returned 0 — assert that without locking in a
    // specific code in case future findings escalate severity.
    let code = output.status.code();
    assert!(
        code == Some(0) || code == Some(2),
        "doctor exit code must be 0 (healthy) or 2 (critical); got: {code:?}",
    );
    let v: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("doctor --json output must parse as JSON: {e}"));
    assert!(v.get("sections").is_some(), "missing sections key: {v}");
    assert!(v.get("mode").is_some(), "missing mode key: {v}");
}

// ---------------------------------------------------------------------------
// Cli root flags — propagation to subcommands
// ---------------------------------------------------------------------------

#[test]
fn cli_db_flag_propagates_to_subcommand() {
    // `--db <path>` is a global flag — when set, the subcommand must
    // open the DB at that path, not the default. We verify by storing
    // a memory and confirming the file appears at the requested path.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("propagated.db");
    ai_memory(&db)
        .args(["--json", "store", "-T", "global-flag-test", "-c", "body"])
        .assert()
        .success();
    assert!(
        db.exists(),
        "--db flag must route the store subcommand to the requested path"
    );
}

#[test]
fn cli_json_flag_propagates_to_subcommand() {
    // `--json` is global. Subcommands that honour it emit JSON instead
    // of text. `stats` is a small one to test against.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("json-flag.db");
    let out = ai_memory(&db)
        .args(["--json", "stats"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let _: serde_json::Value = serde_json::from_slice(&out).unwrap_or_else(|e| {
        panic!(
            "--json must produce parseable JSON for stats; got: {} ({e})",
            String::from_utf8_lossy(&out)
        )
    });
}

#[test]
fn cli_agent_id_flag_propagates_to_subcommand() {
    // `--agent-id <id>` is global. The store subcommand stamps it onto
    // `metadata.agent_id`. We round-trip through `get` to confirm.
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("agent-id.db");
    let store_out = ai_memory(&db)
        .args([
            "--json",
            "--agent-id",
            "pr9g-test-agent",
            "store",
            "-T",
            "agent-stamped",
            "-c",
            "body",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&store_out).unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let get_out = ai_memory(&db)
        .args(["--json", "get", &id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let g: serde_json::Value = serde_json::from_slice(&get_out).unwrap();
    // `get` returns `{memory: {...}, links: [...]}`; agent_id lives at
    // `memory.metadata.agent_id`.
    let agent_id = g["memory"]["metadata"]["agent_id"].as_str().unwrap_or("");
    assert_eq!(
        agent_id, "pr9g-test-agent",
        "global --agent-id must propagate; got: {agent_id} (full: {g})"
    );
}

#[test]
fn cli_db_passphrase_file_flag_accepted() {
    // `--db-passphrase-file <PATH>` is global. On non-sqlcipher builds
    // the env var is read but ignored — what we want to assert here is
    // that the flag *parses* and the dispatch *reads the file* without
    // erroring out. An empty passphrase is rejected; a non-empty one
    // round-trips through to the env without affecting the (standard
    // sqlite) DB. We use `stats` because it's a no-op subcommand that
    // doesn't depend on passphrase contents.
    let tmp = TempDir::new().unwrap();
    let pass = tmp.path().join("passphrase.txt");
    std::fs::write(&pass, b"correct-horse-battery-staple\n").unwrap();
    let db = tmp.path().join("passphrase-flag.db");
    ai_memory(&db)
        .args([
            "--db-passphrase-file",
            pass.to_str().unwrap(),
            "--json",
            "stats",
        ])
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Help text round-trip — every #487 subcommand must print --help cleanly.
// ---------------------------------------------------------------------------

#[test]
fn root_help_succeeds() {
    ai_memory_bare()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("AI-agnostic persistent memory"));
}

#[test]
fn issue_487_subcommands_help_succeeds() {
    // Every subcommand added or modified by issue #487 must accept
    // `--help` and exit 0. If a future PR adds a variant without help
    // text the test catches it.
    for sub in ["boot", "install", "wrap", "logs", "audit", "doctor"] {
        ai_memory_bare().args([sub, "--help"]).assert().success();
    }
}

// ---------------------------------------------------------------------------
// Unknown command — clear error + non-zero exit.
// ---------------------------------------------------------------------------

#[test]
fn unknown_command_fails_with_clear_error() {
    ai_memory_bare()
        .arg("not-a-real-subcommand")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized").or(predicate::str::contains("invalid")));
}
