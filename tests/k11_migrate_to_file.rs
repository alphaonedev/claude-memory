// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K11 — `--config-out PATH` (different file) integration test.
//!
//! Pins the standalone-write branch: when `--config-out` names a path
//! distinct from the loaded config, the migrator writes the rendered
//! `[[permissions.rules]]` block to that file and the file parses
//! cleanly as TOML with the expected `permissions.rules` array.

use ai_memory::cli::CliOutput;
use ai_memory::cli::governance_migrate::run_with_paths;
use tempfile::tempdir;

const FIXTURE: &str = r#"
[governance]

[[governance.policy]]
scope = "ns-a"
action = "write"
role = "ops"
decision = "allow"

[[governance.policy]]
scope = "ns-b"
action = "delete"
agent_id = "carol"
decision = "deny"
"#;

#[test]
fn k11_writes_to_named_file_and_parses_back() {
    let tmp = tempdir().unwrap();
    let in_path = tmp.path().join("in.toml");
    let out_path = tmp.path().join("permissions.toml");
    std::fs::write(&in_path, FIXTURE).unwrap();

    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    {
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        run_with_paths(&in_path, Some(&out_path), false, &mut out)
            .expect("write to named file should succeed");
    }

    // Stdout should report the write; nothing on stderr.
    let s = String::from_utf8(stdout).unwrap();
    assert!(s.contains("wrote 2 migrated rule(s)"), "got: {s}");
    assert!(s.contains(out_path.to_string_lossy().as_ref()), "got: {s}");

    let stderr_str = String::from_utf8(stderr).unwrap();
    assert!(stderr_str.is_empty(), "got: {stderr_str}");

    // Parse the written file as TOML.
    let written = std::fs::read_to_string(&out_path).unwrap();
    let parsed: toml::Value = toml::from_str(&written).expect("written file is valid TOML");
    let rules = parsed["permissions"]["rules"]
        .as_array()
        .expect("permissions.rules array");
    assert_eq!(rules.len(), 2);

    // Spot-check both rules round-trip with the documented mapping.
    assert_eq!(rules[0]["namespace_pattern"].as_str(), Some("ns-a"));
    assert_eq!(rules[0]["op"].as_str(), Some("write"));
    assert_eq!(rules[0]["agent_pattern"].as_str(), Some("ops"));
    assert_eq!(rules[0]["decision"].as_str(), Some("allow"));

    assert_eq!(rules[1]["namespace_pattern"].as_str(), Some("ns-b"));
    assert_eq!(rules[1]["op"].as_str(), Some("delete"));
    assert_eq!(rules[1]["agent_pattern"].as_str(), Some("carol"));
    assert_eq!(rules[1]["decision"].as_str(), Some("deny"));
}
