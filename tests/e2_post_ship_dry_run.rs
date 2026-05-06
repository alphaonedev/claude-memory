// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// E2's post-ship-converge.sh is a bash script. Windows runners don't
// ship bash by default, so the dry-run harness check only runs on
// Unix. The script itself works on any platform with a bash interpreter
// (WSL/Git-Bash); CI just doesn't validate that path.
#![cfg(unix)]

//! v0.7 Track E task E2 — post-ship convergence verification dry-run.
//!
//! `scripts/post-ship-converge.sh` is the runbook script the release
//! captain runs within 1 hour of an F5 release-tag landing. The real
//! run installs the published `ai-memory` crate via
//! `cargo install ai-memory --version <X.Y.Z>` and replays the 6
//! canonical Discovery Gate questions against it.
//!
//! That real run is **not** what this test exercises — we cannot
//! reach out to crates.io from CI on every PR. Instead, this test
//! drives the script with `--dry-run --version 0.7.0`, which skips
//! the install + spawn steps and emits the JSON envelope with
//! `dry_run: true`. The point is to keep the envelope **shape**
//! under CI guard so a future refactor of the script can't silently
//! drop the `verdict` field, the `results[]` array, or the per-question
//! IDs the post-mortem playbook in
//! `docs/v0.7/POST-SHIP-CONVERGENCE.md` references by name.
//!
//! When E1's `scripts/t0-orchestrate.sh` lands, it must reuse the
//! same 6 question IDs (Q1..Q6 with the suffixes asserted below).
//! Drift between the two scripts is itself a bug — both should
//! converge on whatever calibration cells `tests/calibration_t0.rs`
//! pins.

use std::path::PathBuf;
use std::process::Command;

/// Resolve the absolute path to `scripts/post-ship-converge.sh`
/// relative to the crate manifest dir, which `cargo test` always
/// sets to the workspace root.
fn script_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("scripts");
    p.push("post-ship-converge.sh");
    p
}

#[test]
fn e2_dry_run_emits_well_formed_envelope() {
    let script = script_path();
    assert!(
        script.exists(),
        "post-ship-converge.sh missing at {script:?} — E2 deliverable"
    );

    let out = Command::new("bash")
        .arg(&script)
        .arg("--dry-run")
        .arg("--version")
        .arg("0.7.0")
        .output()
        .expect("spawn post-ship-converge.sh");

    assert!(
        out.status.success(),
        "dry-run exit non-zero: status={:?}\nstderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );

    let stdout = String::from_utf8(out.stdout).expect("stdout is utf-8");

    // Parse as JSON — the envelope is meant to be machine-readable so
    // the release-day automation can grep verdict/pass_count out of it.
    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is well-formed JSON envelope");

    // ----- top-level shape -----
    assert_eq!(json["task"], "v0.7-E2", "task tag wrong: {stdout}");
    assert_eq!(json["version"], "0.7.0", "version echo wrong: {stdout}");
    assert_eq!(json["dry_run"], true, "dry_run flag wrong: {stdout}");
    assert_eq!(
        json["verdict"], "DRY_RUN",
        "dry-run verdict wrong: {stdout}"
    );
    assert_eq!(json["question_count"], 6, "question_count wrong: {stdout}");
    assert_eq!(json["pass_count"], 0, "dry-run pass_count must be 0");
    assert_eq!(json["fail_count"], 0, "dry-run fail_count must be 0");
    assert_eq!(
        json["install_method"], "cargo",
        "default install method wrong: {stdout}"
    );

    // ----- per-question results array -----
    let results = json["results"].as_array().expect("results is array");
    assert_eq!(results.len(), 6, "expected 6 questions, got {results:?}");

    // The runbook in docs/v0.7/POST-SHIP-CONVERGENCE.md references
    // these IDs by name. Pin them so a script refactor that renames
    // a cell forces a docs update at the same time.
    let expected_ids = [
        "Q1-T0-A2-CORE",
        "Q2-T0-A2-GRAPH",
        "Q3-T0-A2-FULL",
        "Q4-T0-A1-CORE-RECOVERY-PATHS",
        "Q5-T0-NO-JARGON-FULL",
        "Q6-T0-CONTRACT-CORE",
    ];
    for (i, expected_id) in expected_ids.iter().enumerate() {
        assert_eq!(
            results[i]["id"], *expected_id,
            "question {i} id drift: got={}",
            results[i]["id"]
        );
        assert_eq!(
            results[i]["status"], "SKIPPED_DRY_RUN",
            "dry-run status must be SKIPPED_DRY_RUN for {expected_id}"
        );
    }
}

#[test]
fn e2_dry_run_supports_brew_install_method() {
    // The runbook documents three install methods (cargo / brew /
    // binary). Cover the brew path under --dry-run too so the script's
    // arg parser doesn't regress on the non-default methods. (The real
    // brew install path is exercised manually by the release captain.)
    let out = Command::new("bash")
        .arg(script_path())
        .arg("--dry-run")
        .arg("--version")
        .arg("0.7.1")
        .arg("--method")
        .arg("brew")
        .output()
        .expect("spawn post-ship-converge.sh");

    assert!(out.status.success(), "brew dry-run exit non-zero");
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout JSON envelope");
    assert_eq!(json["install_method"], "brew");
    assert_eq!(json["version"], "0.7.1");
    assert_eq!(json["verdict"], "DRY_RUN");
}

#[test]
fn e2_missing_version_flag_is_usage_error() {
    // Forgetting --version must be a hard usage error (exit 3), not a
    // silent default to "latest". The release captain MUST type the
    // version they expect to verify so they cannot accidentally
    // verify the wrong tag.
    let out = Command::new("bash")
        .arg(script_path())
        .arg("--dry-run")
        .output()
        .expect("spawn post-ship-converge.sh");

    let code = out.status.code().expect("exited normally");
    assert_eq!(code, 3, "missing --version must exit 3 (usage)");
}
