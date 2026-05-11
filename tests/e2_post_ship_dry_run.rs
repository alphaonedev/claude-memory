// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0.1 — closes #625.
//
// E2's `post-ship-converge` is implemented as a cross-platform Rust
// binary at `tools/post-ship-converge/`. The bash variant landed in
// PR #622 (shipped with v0.7.0 as a transitional runbook script) is
// superseded by this binary — this test validates the Rust binary,
// which builds on the fly via `cargo build --manifest-path
// tools/post-ship-converge/Cargo.toml` behind a `OnceLock` to avoid
// the parallel-rebuild flake handled in PR #623.

//! v0.7.0.1 task E2 — minimal harness check on the
//! `post-ship-converge` Rust binary.
//!
//! The verifier is an out-of-band tool that probes the cargo /
//! brew / GitHub-release distribution channels for a freshly-cut
//! release and asserts they all converge on the same version
//! string (see `docs/v0.7/POST-SHIP-CONVERGENCE.md`). Live runs
//! make HTTP calls; CI exercises it in `--dry-run` mode and
//! asserts:
//!
//! 1. The binary builds.
//! 2. `--dry-run` exits 0 without making any network calls.
//! 3. The plan output names all three distribution channels
//!    (cargo / brew / binary).
//! 4. The dry-run advertises the result-file template path.
//! 5. With `--out`, the JSON envelope contains the documented
//!    plan entries with their field names.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

fn converge_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(build_converge_once)
}

fn build_converge_once() -> PathBuf {
    let manifest_path = std::env::current_dir()
        .expect("cwd")
        .join("tools/post-ship-converge/Cargo.toml");
    assert!(
        manifest_path.exists(),
        "post-ship-converge manifest missing at {}",
        manifest_path.display()
    );

    let target_dir = std::env::temp_dir().join(format!(
        "ai-memory-post-ship-converge-target-{}",
        std::process::id()
    ));

    let status = Command::new("cargo")
        .args([
            "build",
            "--quiet",
            "--manifest-path",
            manifest_path.to_str().expect("utf-8 manifest path"),
            "--target-dir",
            target_dir.to_str().expect("utf-8 target dir"),
        ])
        .status()
        .expect("invoke cargo build for post-ship-converge");
    assert!(
        status.success(),
        "cargo build for post-ship-converge failed"
    );

    let bin_name = if cfg!(windows) {
        "post-ship-converge.exe"
    } else {
        "post-ship-converge"
    };
    let bin = target_dir.join("debug").join(bin_name);
    assert!(
        bin.exists(),
        "post-ship-converge binary missing at {}",
        bin.display()
    );
    bin
}

fn run_dry_run(version: &str) -> String {
    let bin = converge_bin();
    let output = Command::new(bin)
        .arg("--dry-run")
        .arg("--version")
        .arg(version)
        .output()
        .expect("spawn post-ship-converge --dry-run");

    assert!(
        output.status.success(),
        "E2: --dry-run exited non-zero (status={:?})\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("dry-run stdout is UTF-8")
}

#[test]
fn e2_dry_run_exits_clean_and_names_all_three_methods() {
    let out = run_dry_run("0.7.0");
    for method in &["cargo", "brew", "binary"] {
        assert!(
            out.contains(&format!("method:   {method}")),
            "E2: dry-run plan missing method={method}\nfull output:\n{out}"
        );
    }
}

#[test]
fn e2_dry_run_advertises_results_template() {
    let out = run_dry_run("0.7.0");
    assert!(
        out.contains("results_template:"),
        "E2: dry-run missing results_template line\nfull output:\n{out}"
    );
    assert!(
        out.contains("results/post-ship/"),
        "E2: results path should sit under results/post-ship/\nfull output:\n{out}"
    );
}

#[test]
fn e2_dry_run_makes_no_network_calls() {
    let out = run_dry_run("0.7.0");
    assert!(
        out.contains("dry-run complete (no network calls made)"),
        "E2: dry-run did not print completion marker\nfull output:\n{out}"
    );
}

#[test]
fn e2_method_filter_restricts_plan_to_one_channel() {
    let bin = converge_bin();
    let output = Command::new(bin)
        .arg("--dry-run")
        .arg("--version")
        .arg("0.7.0")
        .arg("--method")
        .arg("cargo")
        .output()
        .expect("spawn post-ship-converge --dry-run --method cargo");
    assert!(output.status.success(), "E2: --method cargo dry-run failed");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("method:   cargo"));
    assert!(
        !stdout.contains("method:   brew"),
        "E2: --method cargo should exclude brew\n{stdout}"
    );
    assert!(
        !stdout.contains("method:   binary"),
        "E2: --method cargo should exclude binary\n{stdout}"
    );
}

#[test]
fn e2_dry_run_emits_json_envelope_with_plan_entries() {
    let bin = converge_bin();
    let out_dir = std::env::temp_dir().join(format!(
        "ai-memory-post-ship-envelope-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&out_dir).expect("create temp dir");
    let out_path = out_dir.join("plan.json");

    let status = Command::new(bin)
        .arg("--dry-run")
        .arg("--version")
        .arg("0.7.0")
        .arg("--out")
        .arg(&out_path)
        .status()
        .expect("spawn post-ship-converge --dry-run --out");
    assert!(status.success(), "E2: --dry-run --out exited non-zero");

    let body = std::fs::read_to_string(&out_path).expect("read envelope");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("envelope is valid JSON");

    assert_eq!(parsed["mode"], "dry-run");
    assert_eq!(parsed["expected_version"], "0.7.0");
    let plan = parsed["plan"].as_array().expect("plan array");
    assert_eq!(plan.len(), 3, "E2: expected 3 plan entries (3 channels)");
    for entry in plan {
        for field in &["method", "channel", "metadata_url", "expected_version"] {
            assert!(
                entry.get(field).is_some(),
                "E2: plan entry missing field `{field}`: {entry}"
            );
        }
    }
}
