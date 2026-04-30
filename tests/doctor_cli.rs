// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Phase P7 / R7 — `ai-memory doctor` CLI integration tests.
//!
//! These tests spawn the real `ai-memory` binary (via `assert_cmd`) and
//! exercise the doctor subcommand end-to-end. They cover:
//!
//! - `doctor_reports_clean_on_fresh_db` — a freshly-initialized DB
//!   produces a non-critical report.
//! - `doctor_warns_on_dim_violations` — when the post-P2 `embedding_dim`
//!   column is present and a row's dim disagrees with its namespace's
//!   modal dim, the doctor reports CRITICAL on the Storage section. (We
//!   simulate the post-P2 schema by hand-editing the SQLite file; the
//!   real P2 migration lands separately.)
//! - `doctor_critical_on_pending_actions_older_than_24h` — synthesizing
//!   a `pending_actions` row with `requested_at` 25h in the past pushes
//!   Governance into CRITICAL and the process exits 2.
//! - `doctor_remote_queries_capabilities_endpoint` — `--remote <url>`
//!   pulls the Capabilities JSON from a live `serve` daemon and renders
//!   the recall_mode / reranker_active fields.
//!
//! All tests set `AI_MEMORY_NO_CONFIG=1` per the standard CLI test
//! convention.

use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use predicates::prelude::*;
use rusqlite::params;
use tempfile::TempDir;

const SPAWN_TIMEOUT: Duration = Duration::from_secs(15);

/// Build the standard `ai-memory --db <tmp>` command shape with
/// `AI_MEMORY_NO_CONFIG=1` set.
fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

/// Initialize the DB by running an ai-memory command that touches it.
/// We use `stats` because it's a read-only path that triggers
/// `db::open` -> migrations -> close.
fn init_db(db: &Path) {
    ai_memory(db).args(["stats"]).assert().success();
}

#[test]
fn doctor_reports_clean_on_fresh_db() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    init_db(&db);

    // Default invocation — text mode, no --fail-on-warn. A fresh DB has
    // no critical findings and the process exits 0.
    ai_memory(&db)
        .args(["doctor"])
        .assert()
        .success()
        .stdout(predicate::str::contains("ai-memory doctor"))
        .stdout(predicate::str::contains("Storage"))
        .stdout(predicate::str::contains("Governance"))
        .stdout(predicate::str::contains("Webhook"))
        .stdout(predicate::str::contains("overall:      INFO"));

    // JSON mode produces a parseable document with the expected shape.
    let out = ai_memory(&db)
        .args(["doctor", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["mode"].as_str().unwrap(), "local");
    assert!(v["sections"].is_array());
    let sections = v["sections"].as_array().unwrap();
    assert!(!sections.is_empty(), "expected at least one section");
    // No section should be Critical on a fresh DB.
    for s in sections {
        let sev = s["severity"].as_str().unwrap_or("");
        assert_ne!(
            sev, "critical",
            "fresh DB unexpectedly produced critical section: {s}"
        );
    }
}

#[test]
fn doctor_warns_on_dim_violations() {
    // Simulate the post-P2 schema by manually adding the `embedding_dim`
    // column. Pre-P2 the column doesn't exist and `db::doctor_dim_violations`
    // returns Ok(None) so this test would render N/A. Once P2 lands the
    // column natively the manual ALTER becomes a no-op (the existing column
    // check inside the helper is dim-agnostic).
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    init_db(&db);

    let conn = rusqlite::Connection::open(&db).unwrap();
    let _ = conn.execute("ALTER TABLE memories ADD COLUMN embedding_dim INTEGER", []);

    // Insert two rows in the same namespace with different non-null dims —
    // the modal dim becomes whichever appears most. With one 384 and one
    // 768, the helper picks one as modal (ties broken by ordering) and
    // reports the other as a violation. We insert two 384s and one 768
    // so 384 is the unambiguous mode and the lone 768 is the violation.
    let now = chrono::Utc::now().to_rfc3339();
    for (id, dim) in &[("dim-a", 384_i64), ("dim-b", 384_i64), ("dim-c", 768_i64)] {
        conn.execute(
            "INSERT INTO memories (id, tier, namespace, title, content, tags, priority,
                                   confidence, source, access_count, created_at, updated_at,
                                   embedding, embedding_dim, metadata)
             VALUES (?1, 'long', 'p2-test', ?2, 'content', '[]', 5, 1.0, 'test', 0, ?3, ?3,
                     X'01000000', ?4, '{}')",
            params![id, format!("title-{id}"), now, dim],
        )
        .unwrap();
    }
    drop(conn);

    // The doctor should now flag dim_violations >= 1 and exit 2 (Critical).
    let assert = ai_memory(&db).args(["doctor"]).assert();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("dim_violations"),
        "expected dim_violations in stdout, got: {stdout}"
    );
    assert!(
        stdout.contains("[CRIT] Storage") || stdout.contains("CRIT"),
        "expected CRIT severity, got: {stdout}"
    );
    // Exit code: 2 (Critical).
    let code = output.status.code().unwrap_or(0);
    assert_eq!(code, 2, "expected exit code 2 (critical), got {code}");
}

#[test]
fn doctor_critical_on_pending_actions_older_than_24h() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    init_db(&db);

    // Insert a `pending_actions` row dated 25 hours in the past.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let twenty_five_hours_ago = (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
    conn.execute(
        "INSERT INTO pending_actions (id, action_type, namespace, payload, requested_by,
                                       requested_at, status)
         VALUES ('stale-1', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
        params![twenty_five_hours_ago],
    )
    .unwrap();
    drop(conn);

    let assert = ai_memory(&db).args(["doctor"]).assert();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Governance"),
        "expected Governance section, got: {stdout}"
    );
    assert!(
        stdout.contains("oldest_pending_age_secs"),
        "expected oldest_pending_age_secs fact, got: {stdout}"
    );
    assert!(
        stdout.contains("CRIT"),
        "expected CRIT severity, got: {stdout}"
    );
    let code = output.status.code().unwrap_or(0);
    assert_eq!(code, 2, "expected exit code 2 (critical), got {code}");
}

#[test]
fn doctor_remote_queries_capabilities_endpoint() {
    let tmp = TempDir::new().unwrap();
    let db = tmp.path().join("ai-memory.db");
    let serve = spawn_serve(&db);

    let assert = ai_memory(&db)
        .args(["doctor", "--remote", &serve.base_url()])
        .assert();
    let output = assert.get_output();
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Smoke checks on the rendered text:
    //   - mode is "remote"
    //   - source contains the host URL
    //   - Capabilities section is present and not Critical (the live
    //     daemon reaches the endpoint, so even a v1 schema renders Info)
    assert!(
        stdout.contains("remote mode"),
        "expected 'remote mode', got: {stdout}"
    );
    assert!(
        stdout.contains("Capabilities"),
        "expected Capabilities section, got: {stdout}"
    );
    assert!(
        stdout.contains("schema_version"),
        "expected schema_version fact, got: {stdout}"
    );

    // Should exit 0: the live daemon's capabilities endpoint is reachable
    // and recall_mode_active either matches "hybrid" or is absent (v1
    // schema), neither of which trips a Warning on this clean test daemon.
    let code = output.status.code().unwrap_or(0);
    assert_eq!(code, 0, "expected exit 0 from healthy remote, got {code}");

    // JSON mode parses cleanly.
    let json_out = ai_memory(&db)
        .args(["doctor", "--remote", &serve.base_url(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json_out).unwrap();
    assert_eq!(v["mode"].as_str().unwrap(), "remote");
}

// ---------------------------------------------------------------------------
// Local serve helper (lifted from tests/serve_integration.rs to keep this
// file self-contained — the helper there is private and not exported).
// ---------------------------------------------------------------------------

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
    listener.local_addr().expect("local_addr").port()
}

struct ServeChild {
    child: Option<Child>,
    port: u16,
}

impl ServeChild {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for ServeChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn spawn_serve(db: &Path) -> ServeChild {
    let port = free_port();
    let port_s = port.to_string();
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_ai-memory"));
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args([
            "--db",
            db.to_str().unwrap(),
            "serve",
            "--host",
            "127.0.0.1",
            "--port",
            &port_s,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn ai-memory serve");

    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || for _ in BufReader::new(stdout).lines() {});
    }
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || for _ in BufReader::new(stderr).lines() {});
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{port}/api/v1/health");
    let deadline = Instant::now() + SPAWN_TIMEOUT;
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(&url).send()
            && resp.status().is_success()
        {
            return ServeChild {
                child: Some(child),
                port,
            };
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("serve child exited before /health became ready: {status}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    panic!("serve daemon did not become ready within {SPAWN_TIMEOUT:?}");
}
