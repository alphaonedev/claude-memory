// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 WT-1-F — integration tests for `ai-memory atomise`.
//!
//! Exercises the CLI handler via [`run_with_curator`], plugging in a
//! deterministic mock curator so the suite never burns an LLM round-trip.
//! The substrate engine is the same one [`tests/atomisation/core.rs`]
//! pins; these tests stay narrow on the CLI-layer concerns (exit codes,
//! human + JSON output, tier gating).

#![allow(clippy::doc_markdown, clippy::too_many_lines)]

use ai_memory::models::ConfidenceSource;
use std::path::Path;
use std::sync::Mutex;

use ai_memory::atomisation::curator::{Atom, Curator, CuratorError};
use ai_memory::cli::CliOutput;
use ai_memory::cli::commands::atomise::{self, AtomiseArgs};
use ai_memory::config::{AppConfig, FeatureTier};
use ai_memory::db;
use ai_memory::models::{Memory, MemoryKind, Tier};

use chrono::Utc;
use tempfile::TempDir;

// ─────────────────────────────────────────────────────────────────────
// Mock curator
// ─────────────────────────────────────────────────────────────────────

struct MockCurator {
    responses: Mutex<Vec<Result<Vec<Atom>, CuratorError>>>,
}

impl MockCurator {
    fn new(responses: Vec<Result<Vec<Atom>, CuratorError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl Curator for MockCurator {
    fn decompose(
        &self,
        _body: &str,
        _max_atom_tokens: u32,
        _max_retries: u32,
    ) -> Result<Vec<Atom>, CuratorError> {
        let mut q = self.responses.lock().unwrap();
        if q.is_empty() {
            return Err(CuratorError::MalformedResponse(
                "mock: queue exhausted".into(),
            ));
        }
        q.remove(0)
    }
}

fn atoms(texts: &[&str]) -> Vec<Atom> {
    texts
        .iter()
        .map(|s| Atom {
            text: (*s).to_string(),
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers
// ─────────────────────────────────────────────────────────────────────

fn fresh_db() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("ai-memory.db");
    // Open once to materialise schema, drop the conn before the CLI
    // re-opens it from `db_path`.
    {
        let _conn = db::open(&db_path).expect("db::open");
    }
    (tmp, db_path)
}

fn insert_long_source(conn: &rusqlite::Connection, ns: &str) -> String {
    let now = Utc::now().to_rfc3339();
    // Body well over 200 tokens so SourceTooSmall doesn't fire.
    let body = (0..30)
        .map(|i| {
            format!(
                "Paragraph {i}: the kubernetes rolling deploy strategy required canary \
                 instance health checks. The pod readiness probe must pass before \
                 traffic shifts. Failures roll back the deployment within 30 seconds. \
                 Operator dashboards track replica counts and error rates."
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let mem = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: ns.to_string(),
        title: format!("source-{}", uuid::Uuid::new_v4().simple()),
        content: body,
        tags: vec!["kubernetes".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    };
    db::insert(conn, &mem).expect("seed source")
}

fn make_args(memory_id: &str) -> AtomiseArgs {
    AtomiseArgs {
        memory_id: memory_id.to_string(),
        max_atom_tokens: 200,
        force: false,
        json: false,
        quiet: false,
    }
}

/// Build a minimal `AppConfig` and apply a tier override via the config
/// `tier` field.
fn app_config_with_tier(tier: FeatureTier) -> AppConfig {
    AppConfig {
        tier: Some(tier.as_str().to_string()),
        ..AppConfig::default()
    }
}

fn run_with_mock(
    db_path: &Path,
    args: &AtomiseArgs,
    tier: FeatureTier,
    mock: Box<dyn Curator>,
) -> (i32, String, String) {
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut out = CliOutput {
        stdout: &mut stdout,
        stderr: &mut stderr,
    };
    let cfg = app_config_with_tier(tier);
    let rc = atomise::run_with_curator(
        db_path,
        args,
        &cfg,
        Some("test-agent"),
        &mut out,
        Some(mock),
    )
    .expect("run_with_curator");
    (
        rc,
        String::from_utf8(stdout).expect("stdout utf8"),
        String::from_utf8(stderr).expect("stderr utf8"),
    )
}

// ─────────────────────────────────────────────────────────────────────
// Test 1 — success path: exit 0, output contains atom_count
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_success() {
    let (_tmp, db_path) = fresh_db();
    let source_id = {
        let conn = db::open(&db_path).expect("open");
        insert_long_source(&conn, "ns-success")
    };

    let mock = Box::new(MockCurator::new(vec![Ok(atoms(&[
        "atom one is short and self-contained.",
        "atom two captures the rollback condition.",
        "atom three covers operator dashboards.",
    ]))]));

    let args = make_args(&source_id);
    let (rc, stdout, stderr) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock);

    assert_eq!(rc, 0, "expected exit 0, got {rc}; stderr={stderr}");
    assert!(stderr.is_empty(), "stderr must be empty: {stderr}");
    assert!(
        stdout.contains("3 atoms"),
        "expected atom_count in stdout: {stdout}"
    );
    assert!(stdout.contains(&source_id), "expected source id in stdout");
    assert!(
        stdout.contains("archived at"),
        "expected archived_at timestamp in stdout"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Test 2 — not found: exit 2
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_not_found() {
    let (_tmp, db_path) = fresh_db();

    let mock = Box::new(MockCurator::new(vec![]));
    let args = make_args("00000000-0000-0000-0000-000000000000");
    let (rc, stdout, stderr) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock);

    assert_eq!(rc, 2, "expected exit 2 for not_found");
    assert!(stdout.is_empty(), "stdout must be empty on error");
    assert!(
        stderr.contains("not found"),
        "expected 'not found' in stderr: {stderr}"
    );
    assert!(stderr.contains("00000000-0000-0000-0000-000000000000"));
}

// ─────────────────────────────────────────────────────────────────────
// Test 3 — keyword tier: exit 3
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_keyword_tier() {
    let (_tmp, db_path) = fresh_db();
    // No source needed — tier check fires before DB read.
    let mock = Box::new(MockCurator::new(vec![]));
    let args = make_args("any-id");
    let (rc, stdout, stderr) = run_with_mock(&db_path, &args, FeatureTier::Keyword, mock);

    assert_eq!(rc, 3, "expected exit 3 for tier_locked");
    assert!(stdout.is_empty());
    assert!(
        stderr.contains("requires smart tier"),
        "stderr must hint at tier upgrade: {stderr}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Test 4 — already atomised without force: exit 1
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_already_atomised_without_force() {
    let (_tmp, db_path) = fresh_db();
    let source_id = {
        let conn = db::open(&db_path).expect("open");
        insert_long_source(&conn, "ns-already")
    };

    // First run — succeeds.
    let mock1 = Box::new(MockCurator::new(vec![Ok(atoms(&[
        "atom a", "atom b", "atom c",
    ]))]));
    let args = make_args(&source_id);
    let (rc1, _stdout1, _stderr1) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock1);
    assert_eq!(rc1, 0, "first atomise must succeed");

    // Second run — without --force.
    let mock2 = Box::new(MockCurator::new(vec![Ok(atoms(&["x", "y"]))]));
    let (rc2, stdout2, stderr2) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock2);

    assert_eq!(rc2, 1, "expected exit 1 for AlreadyAtomised");
    assert!(stdout2.is_empty(), "stdout must be empty on info");
    assert!(
        stderr2.contains("already atomised"),
        "expected 'already atomised' in stderr: {stderr2}"
    );
    assert!(
        stderr2.contains("--force"),
        "stderr must mention --force as the remediation"
    );
    assert!(
        stderr2.contains("3 atoms"),
        "stderr must surface existing atom_count: {stderr2}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// Test 5 — force re-atomises: new atom_ids
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_force_re_atomises() {
    let (_tmp, db_path) = fresh_db();
    let source_id = {
        let conn = db::open(&db_path).expect("open");
        insert_long_source(&conn, "ns-force")
    };

    // First run, capture the atom ids.
    let mock1 = Box::new(MockCurator::new(vec![Ok(atoms(&[
        "atom 1", "atom 2", "atom 3",
    ]))]));
    let args1 = AtomiseArgs {
        memory_id: source_id.clone(),
        max_atom_tokens: 200,
        force: false,
        json: true,
        quiet: false,
    };
    let (rc1, stdout1, _) = run_with_mock(&db_path, &args1, FeatureTier::Smart, mock1);
    assert_eq!(rc1, 0);
    let v1: serde_json::Value = serde_json::from_str(stdout1.trim()).expect("json parse");
    let first_ids: Vec<String> = v1["atom_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(first_ids.len(), 3);

    // Second run with --force: fresh atom_ids.
    let mock2 = Box::new(MockCurator::new(vec![Ok(atoms(&[
        "atom 4", "atom 5", "atom 6", "atom 7",
    ]))]));
    let args2 = AtomiseArgs {
        memory_id: source_id.clone(),
        max_atom_tokens: 200,
        force: true,
        json: true,
        quiet: false,
    };
    let (rc2, stdout2, stderr2) = run_with_mock(&db_path, &args2, FeatureTier::Smart, mock2);
    assert_eq!(rc2, 0, "force re-atomise must succeed: stderr={stderr2}");
    let v2: serde_json::Value = serde_json::from_str(stdout2.trim()).expect("json parse");
    let second_ids: Vec<String> = v2["atom_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap().to_string())
        .collect();
    assert_eq!(second_ids.len(), 4);

    // Atom ids must be disjoint (fresh uuids on the re-atomise path).
    for id in &second_ids {
        assert!(
            !first_ids.contains(id),
            "force re-atomise must mint fresh atom ids, but {id} was reused"
        );
    }
}

// ─────────────────────────────────────────────────────────────────────
// Test 6 — JSON output: parse, assert fields present
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_json_output() {
    let (_tmp, db_path) = fresh_db();
    let source_id = {
        let conn = db::open(&db_path).expect("open");
        insert_long_source(&conn, "ns-json")
    };

    let mock = Box::new(MockCurator::new(vec![Ok(atoms(&[
        "json atom 1.",
        "json atom 2.",
    ]))]));
    let args = AtomiseArgs {
        memory_id: source_id.clone(),
        max_atom_tokens: 200,
        force: false,
        json: true,
        quiet: false,
    };
    let (rc, stdout, stderr) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock);
    assert_eq!(rc, 0, "stderr={stderr}");
    assert!(stderr.is_empty());
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("stdout must be valid JSON");
    assert_eq!(v["source_id"].as_str().unwrap(), source_id);
    assert_eq!(v["atom_count"].as_i64().unwrap(), 2);
    assert_eq!(v["atom_ids"].as_array().unwrap().len(), 2);
    assert!(
        v["archived_at"].as_str().unwrap().contains('T'),
        "archived_at must be RFC3339 (contain 'T'): {}",
        v["archived_at"]
    );
}

// ─────────────────────────────────────────────────────────────────────
// Test 7 — source_too_small classifies as informational (exit 1)
// ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cli_atomise_source_too_small_returns_informational() {
    let (_tmp, db_path) = fresh_db();
    let short_id = {
        let conn = db::open(&db_path).expect("open");
        let now = Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "ns-tiny".to_string(),
            title: format!("tiny-{}", uuid::Uuid::new_v4().simple()),
            content: "Short body that fits within one atom budget.".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"agent_id": "test-agent"}),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
            confidence_source: ConfidenceSource::CallerProvided,
            confidence_signals: None,
            confidence_decayed_at: None,
            version: 1,
        };
        db::insert(&conn, &mem).expect("seed tiny")
    };

    // Curator queue empty — source is too small, curator round-trip
    // never fires.
    let mock = Box::new(MockCurator::new(vec![]));
    let args = make_args(&short_id);
    let (rc, stdout, stderr) = run_with_mock(&db_path, &args, FeatureTier::Smart, mock);
    assert_eq!(rc, 1, "SourceTooSmall is informational (exit 1)");
    assert!(stdout.is_empty());
    assert!(stderr.contains(&short_id), "stderr must echo memory id");
    assert!(
        stderr.contains("max_atom_tokens"),
        "stderr must hint at the token budget: {stderr}"
    );
}
