// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K11 — `ai-memory governance migrate-to-permissions` dry-run
//! integration test.
//!
//! Pins the default behaviour: load a fixture `config.toml` carrying
//! three `[[governance.policy]]` entries, run the migrator with no
//! `--config-out`, and assert the rendered `[[permissions.rules]]`
//! block lands on stdout with the documented K9 field mapping.

use ai_memory::cli::CliOutput;
use ai_memory::cli::governance_migrate::run_with_paths;
use tempfile::tempdir;

const FIXTURE: &str = r#"
# v0.6.x style config carrying a [governance] section

tier = "semantic"

[governance]

[[governance.policy]]
scope = "team/eng/*"
action = "write"
role = "engineer"
decision = "allow"

[[governance.policy]]
scope = "team/finance/*"
action = "delete"
agent_id = "alice"
decision = "ask"

[[governance.policy]]
scope = "*"
action = "promote"
decision = "deny"
"#;

#[test]
fn k11_dry_run_emits_three_rules_to_stdout() {
    let tmp = tempdir().unwrap();
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, FIXTURE).unwrap();

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    {
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        run_with_paths(&cfg, None, true, &mut out).expect("dry-run should succeed");
    }

    let s = String::from_utf8(stdout).unwrap();
    assert_eq!(
        s.matches("[[permissions.rules]]").count(),
        3,
        "expected 3 rule blocks, got: {s}"
    );

    // Field mapping: scope → namespace_pattern.
    assert!(s.contains("namespace_pattern = \"team/eng/*\""), "got: {s}");
    assert!(
        s.contains("namespace_pattern = \"team/finance/*\""),
        "got: {s}"
    );
    assert!(s.contains("namespace_pattern = \"*\""), "got: {s}");

    // Field mapping: action → op.
    assert!(s.contains("op = \"write\""), "got: {s}");
    assert!(s.contains("op = \"delete\""), "got: {s}");
    assert!(s.contains("op = \"promote\""), "got: {s}");

    // Field mapping: role wins over agent_id; agent_id used as fallback.
    assert!(s.contains("agent_pattern = \"engineer\""), "got: {s}");
    assert!(s.contains("agent_pattern = \"alice\""), "got: {s}");

    // Field mapping: decision forwarded verbatim.
    assert!(s.contains("decision = \"allow\""), "got: {s}");
    assert!(s.contains("decision = \"ask\""), "got: {s}");
    assert!(s.contains("decision = \"deny\""), "got: {s}");

    // Stderr stays quiet on the happy path (no warnings).
    let err = String::from_utf8(stderr).unwrap();
    assert!(err.is_empty(), "expected empty stderr, got: {err}");
}
