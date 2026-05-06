// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K11 — `--config-out PATH` (same as loaded config) integration
//! test.
//!
//! Pins the in-place merge branch: when `--config-out` matches the
//! input path, the migrator must preserve every other section of the
//! original config verbatim and append the new
//! `[[permissions.rules]]` block.

use ai_memory::cli::CliOutput;
use ai_memory::cli::governance_migrate::run_with_paths;
use tempfile::tempdir;

const FIXTURE: &str = r#"# preamble
tier = "semantic"
db = "ai-memory.db"

[scoring]
legacy_scoring = false

[ttl]
short_secs = 21600

[governance]

[[governance.policy]]
scope = "team/eng/*"
action = "write"
role = "engineer"
decision = "allow"

[[governance.policy]]
scope = "*"
action = "delete"
decision = "ask"
"#;

#[test]
fn k11_in_place_merge_preserves_other_sections() {
    let tmp = tempdir().unwrap();
    let cfg = tmp.path().join("config.toml");
    std::fs::write(&cfg, FIXTURE).unwrap();

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    {
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        run_with_paths(&cfg, Some(&cfg), false, &mut out).expect("in-place merge should succeed");
    }

    let after = std::fs::read_to_string(&cfg).unwrap();

    // Other sections preserved verbatim.
    assert!(after.contains("tier = \"semantic\""), "got: {after}");
    assert!(after.contains("db = \"ai-memory.db\""), "got: {after}");
    assert!(after.contains("[scoring]"), "got: {after}");
    assert!(after.contains("legacy_scoring = false"), "got: {after}");
    assert!(after.contains("[ttl]"), "got: {after}");
    assert!(after.contains("short_secs = 21600"), "got: {after}");

    // Original [governance] block is preserved (additive, not destructive).
    assert!(after.contains("[governance]"), "got: {after}");
    assert!(after.contains("[[governance.policy]]"), "got: {after}");

    // New [[permissions.rules]] entries appended.
    assert_eq!(after.matches("[[permissions.rules]]").count(), 2);
    assert!(after.contains("namespace_pattern = \"team/eng/*\""));
    assert!(after.contains("agent_pattern = \"engineer\""));
    assert!(after.contains("decision = \"allow\""));

    // The migration banner is the marker an operator looks for to find
    // the appended block.
    assert!(
        after.contains("migrated from [governance] (v0.7.0 K11)"),
        "merge banner missing: {after}"
    );

    // The merged result must still be valid TOML (the K11 contract is
    // "operators paste back the loaded config and it parses").
    let parsed: toml::Value = toml::from_str(&after).expect("merged file is valid TOML");
    let rules = parsed["permissions"]["rules"]
        .as_array()
        .expect("permissions.rules array");
    assert_eq!(rules.len(), 2);

    // Stdout reports the rule count, stderr stays quiet.
    let s = String::from_utf8(stdout).unwrap();
    assert!(s.contains("wrote 2 migrated rule(s)"), "got: {s}");
    let stderr_str = String::from_utf8(stderr).unwrap();
    assert!(stderr_str.is_empty(), "got: {stderr_str}");
}
