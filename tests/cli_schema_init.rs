// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Wave-1 Fix 3 — integration tests for the `ai-memory
//! schema-init` CLI verb.
//!
//! The verb opens a SAL backend by URL (`sqlite://` or `postgres://`),
//! triggers `INIT_SCHEMA` as a side effect, enumerates the resulting
//! catalog, and emits a JSON or human summary.
//!
//! ## Coverage
//!
//! 1. **`schema_init_sqlite_emits_json`** — drives the verb against a
//!    tempfile sqlite URL with `--json`. Asserts the parsed payload has
//!    `kind = "sqlite"`, `schema_version > 0`, and the user tables
//!    `memories` + `memory_links` are present. Pins the JSON wire shape
//!    for downstream tooling (CI, Terraform, etc.).
//!
//! 2. **`schema_init_sqlite_human_output`** — drives the same target
//!    without `--json` and asserts the seven human-readable rows land
//!    on stdout (`schema initialized at`, `tables:`, `indices:`,
//!    `views:`, `functions:`, `extensions:`, `schema_version:`).
//!
//! 3. **`schema_init_sqlite_idempotent_on_rerun`** — runs the verb
//!    twice against the same path. Both must succeed and report
//!    matching `schema_version`.
//!
//! 4. **`schema_init_rejects_bad_url`** — passes a `nosql://...` URL.
//!    The process exits non-zero and stderr carries the
//!    `unrecognised store URL` diagnostic.
//!
//! 5. **`schema_init_postgres_emits_json`** (gated by
//!    `AI_MEMORY_TEST_POSTGRES_URL`) — runs against a live Postgres,
//!    asserts `kind = "postgres"`, `schema_version` > 0, and the
//!    `memories` table is present. When `AI_MEMORY_TEST_AGE_URL` is
//!    *also* set, additionally asserts `age_projection_created = true`
//!    AND `extensions` includes `"age"`.
//!
//! Pattern mirrors `tests/doctor_cli.rs` and `tests/cli_integration.rs`
//! — every test sets `AI_MEMORY_NO_CONFIG=1` and routes through
//! `assert_cmd::Command` so the real built binary is exercised.

#![cfg(feature = "sal")]
#![allow(clippy::zombie_processes)]

use std::path::Path;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

/// Build the standard `ai-memory --db <tmp>` command shape with
/// `AI_MEMORY_NO_CONFIG=1` set. The `--db` arg is required by clap as
/// a global flag even for verbs (like schema-init) that operate on a
/// different `--store-url`; we point it at a disposable tempfile to
/// keep the operator's main DB untouched.
fn ai_memory(db: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ai-memory").unwrap();
    cmd.env("AI_MEMORY_NO_CONFIG", "1")
        .args(["--db", db.to_str().unwrap()]);
    cmd
}

#[test]
fn schema_init_sqlite_emits_json() {
    let tmp = TempDir::new().unwrap();
    let main_db = tmp.path().join("ai-memory.db");
    let target_db = tmp.path().join("schema-init-target.db");
    let url = format!("sqlite://{}", target_db.to_string_lossy());

    let out = ai_memory(&main_db)
        .args(["schema-init", "--store-url", &url, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let raw = String::from_utf8(out).expect("utf-8 stdout");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("expected parseable JSON, got: {raw}\nerror: {e}");
    });

    assert_eq!(v["kind"], "sqlite", "wrong kind: {v}");
    assert_eq!(v["url"], serde_json::Value::String(url), "url mismatch");
    let schema_version = v["schema_version"].as_i64().expect("schema_version i64");
    assert!(
        schema_version > 0,
        "schema_version should be > 0 after init, got {schema_version}"
    );

    let tables: Vec<&str> = v["tables"]
        .as_array()
        .expect("tables array")
        .iter()
        .map(|t| t.as_str().expect("table name str"))
        .collect();
    assert!(
        tables.contains(&"memories"),
        "memories table missing: {tables:?}"
    );
    assert!(
        tables.contains(&"memory_links"),
        "memory_links table missing: {tables:?}"
    );

    // SQLite has no extensions / functions surface; both arrays must
    // be empty so the JSON shape stays stable.
    assert!(
        v["extensions"]
            .as_array()
            .expect("extensions array")
            .is_empty(),
        "sqlite extensions should be empty: {v}"
    );
    assert!(
        v["functions"]
            .as_array()
            .expect("functions array")
            .is_empty(),
        "sqlite functions should be empty: {v}"
    );
    assert_eq!(
        v["age_projection_created"],
        serde_json::Value::Bool(false),
        "age_projection_created must be false for sqlite"
    );
}

#[test]
fn schema_init_sqlite_human_output() {
    let tmp = TempDir::new().unwrap();
    let main_db = tmp.path().join("ai-memory.db");
    let target_db = tmp.path().join("schema-init-target.db");
    let url = format!("sqlite://{}", target_db.to_string_lossy());

    ai_memory(&main_db)
        .args(["schema-init", "--store-url", &url])
        .assert()
        .success()
        .stdout(predicate::str::contains("schema initialized at"))
        .stdout(predicate::str::contains("tables:"))
        .stdout(predicate::str::contains("indices:"))
        .stdout(predicate::str::contains("views:"))
        .stdout(predicate::str::contains("functions:"))
        .stdout(predicate::str::contains("extensions:"))
        .stdout(predicate::str::contains("schema_version:"));
}

#[test]
fn schema_init_sqlite_idempotent_on_rerun() {
    let tmp = TempDir::new().unwrap();
    let main_db = tmp.path().join("ai-memory.db");
    let target_db = tmp.path().join("schema-init-target.db");
    let url = format!("sqlite://{}", target_db.to_string_lossy());

    // First run.
    let out1 = ai_memory(&main_db)
        .args(["schema-init", "--store-url", &url, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v1: serde_json::Value = serde_json::from_slice(&out1).unwrap();
    let ver1 = v1["schema_version"].as_i64().unwrap();

    // Second run against the same path — must succeed and report the
    // same schema_version (idempotence is a load-bearing property of
    // the verb).
    let out2 = ai_memory(&main_db)
        .args(["schema-init", "--store-url", &url, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
    let ver2 = v2["schema_version"].as_i64().unwrap();

    assert_eq!(
        ver1, ver2,
        "schema_version should be identical across reruns: {ver1} vs {ver2}"
    );
}

#[test]
fn schema_init_rejects_bad_url() {
    let tmp = TempDir::new().unwrap();
    let main_db = tmp.path().join("ai-memory.db");

    ai_memory(&main_db)
        .args(["schema-init", "--store-url", "nosql://nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognised store URL"));
}

// ---------------------------------------------------------------------------
// Postgres — gated by env vars so default `cargo test` stays green
// without a Postgres reachable at localhost.
// ---------------------------------------------------------------------------

#[cfg(feature = "sal-postgres")]
#[test]
fn schema_init_postgres_emits_json() {
    let Some(url) = std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok() else {
        eprintln!("skipping schema_init_postgres_emits_json: AI_MEMORY_TEST_POSTGRES_URL unset");
        return;
    };

    let tmp = TempDir::new().unwrap();
    let main_db = tmp.path().join("ai-memory.db");

    let out = ai_memory(&main_db)
        .args(["schema-init", "--store-url", &url, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let raw = String::from_utf8(out).expect("utf-8 stdout");
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or_else(|e| {
        panic!("expected parseable JSON, got: {raw}\nerror: {e}");
    });

    assert_eq!(v["kind"], "postgres", "wrong kind: {v}");
    assert!(
        v["schema_version"].as_i64().unwrap() > 0,
        "schema_version should be > 0 after init: {v}"
    );

    let tables: Vec<&str> = v["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap())
        .collect();
    assert!(
        tables.contains(&"memories"),
        "memories table missing: {tables:?}"
    );

    // When the operator points us at the AGE-enabled fixture, the
    // verb MUST bootstrap `memory_graph` AND surface `age` in the
    // extensions list. Both checks fire when `AI_MEMORY_TEST_AGE_URL`
    // is set so the operator can wire one or the other independently.
    if std::env::var("AI_MEMORY_TEST_AGE_URL").is_ok() {
        let extensions: Vec<&str> = v["extensions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t.as_str().unwrap())
            .collect();
        assert!(
            extensions.contains(&"age"),
            "age extension missing on AGE fixture: {extensions:?}"
        );
        assert_eq!(
            v["age_projection_created"],
            serde_json::Value::Bool(true),
            "age_projection_created must be true when AGE is installed: {v}"
        );
    }
}
