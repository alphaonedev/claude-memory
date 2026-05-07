// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0.1 — closes #625.
//
// E1's bash script (`scripts/t0-orchestrate.sh`) was replaced by a
// cross-platform Rust binary at `tools/t0-orchestrate/`. The
// `#![cfg(unix)]` gate is gone — Windows runners now validate the
// same harness shape macOS/Linux do, because there's no longer a
// `bash` runtime dependency. The binary builds on the fly via
// `cargo build --manifest-path tools/t0-orchestrate/Cargo.toml`,
// behind a `OnceLock` to avoid the parallel-rebuild flake handled
// in PR #623.

//! v0.7.0.1 task E1 — minimal harness check on the
//! `t0-orchestrate` Rust binary.
//!
//! The orchestrator is an out-of-band tool that fans the
//! Discovery Gate questions out to four live LLMs (see
//! `docs/v0.7/T0-ORCHESTRATION.md`). Live runs cost API budget
//! and require keys, so CI exercises it in `--dry-run` mode and
//! asserts:
//!
//! 1. The binary builds.
//! 2. `--dry-run` exits 0 without making API calls.
//! 3. The plan output names all four LLMs (claude / gpt5 /
//!    gemini / grok).
//! 4. The plan output names every Discovery Gate question id
//!    pinned in `tests/calibration_t0.rs`.
//! 5. The dry-run advertises the result-file template paths.
//! 6. With `--out`, the JSON envelope contains 24 plan entries
//!    (4 LLMs × 6 questions) with the documented field names.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Build the `t0-orchestrate` binary once for the test process
/// and return its absolute path. Same `OnceLock` pattern the G11
/// auto-link-detector test adopted in PR #623 — historically the
/// per-fn `cargo build` calls raced under `--test-threads > 1`
/// and tripped `ETXTBSY` on macOS.
fn orchestrate_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(build_orchestrate_once)
}

fn build_orchestrate_once() -> PathBuf {
    let manifest_path = std::env::current_dir()
        .expect("cwd")
        .join("tools/t0-orchestrate/Cargo.toml");
    assert!(
        manifest_path.exists(),
        "t0-orchestrate manifest missing at {}",
        manifest_path.display()
    );

    // Per-PID target dir so two concurrent `cargo test` driver
    // processes (CI sharding) cannot stomp each other's target/.
    let target_dir = std::env::temp_dir().join(format!(
        "ai-memory-t0-orchestrate-target-{}",
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
        .expect("invoke cargo build for t0-orchestrate");
    assert!(status.success(), "cargo build for t0-orchestrate failed");

    let bin_name = if cfg!(windows) {
        "t0-orchestrate.exe"
    } else {
        "t0-orchestrate"
    };
    let bin = target_dir.join("debug").join(bin_name);
    assert!(
        bin.exists(),
        "t0-orchestrate binary missing at {}",
        bin.display()
    );
    bin
}

fn run_dry_run() -> String {
    let bin = orchestrate_bin();
    let output = Command::new(bin)
        .arg("--dry-run")
        .output()
        .expect("spawn t0-orchestrate --dry-run");

    assert!(
        output.status.success(),
        "E1: --dry-run exited non-zero (status={:?})\nstderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout).expect("dry-run stdout is UTF-8")
}

#[test]
fn e1_dry_run_exits_clean_and_names_all_four_llms() {
    let out = run_dry_run();
    for llm in &["claude", "gpt5", "gemini", "grok"] {
        assert!(
            out.contains(&format!("llm:      {llm}")),
            "E1: dry-run plan missing llm={llm}\nfull output:\n{out}"
        );
    }
}

#[test]
fn e1_dry_run_covers_every_calibration_cell_id() {
    let out = run_dry_run();

    // Question ids must match the calibration cells in
    // tests/calibration_t0.rs. If a new cell lands there, add
    // the id to QUESTIONS in tools/t0-orchestrate/src/main.rs
    // and to this list.
    for qid in &[
        "T0-A2-CORE",
        "T0-A2-FULL",
        "T0-A2-GRAPH",
        "T0-A2-NJG",
        "T0-A1-CORE",
        "T0-CONTRACT",
    ] {
        assert!(
            out.contains(qid),
            "E1: dry-run plan missing calibration cell id={qid}\nfull output:\n{out}"
        );
    }
}

#[test]
fn e1_dry_run_advertises_result_file_template() {
    let out = run_dry_run();
    assert!(
        out.contains("results_template:"),
        "E1: dry-run missing results_template line\nfull output:\n{out}"
    );
    assert!(
        out.contains("summary_template:"),
        "E1: dry-run missing summary_template line\nfull output:\n{out}"
    );
    assert!(
        out.contains("results/t0/"),
        "E1: dry-run results path should sit under results/t0/\nfull output:\n{out}"
    );
}

#[test]
fn e1_dry_run_makes_no_api_calls() {
    // Sanity: dry-run must terminate with the explicit marker so
    // we never confuse a silent abort with a clean dry-run.
    let out = run_dry_run();
    assert!(
        out.contains("dry-run complete (no API calls made)"),
        "E1: dry-run did not print completion marker\nfull output:\n{out}"
    );
}

#[test]
fn e1_dry_run_emits_24_entry_json_envelope() {
    // 4 LLMs × 6 Discovery Gate questions = 24 plan entries.
    // The Rust binary writes the envelope to the path supplied
    // by `--out` (the bash script never produced a JSON
    // envelope; this is the new contract called out in #625's
    // acceptance criteria so downstream tooling can consume the
    // plan directly).
    let bin = orchestrate_bin();
    let out_dir = std::env::temp_dir().join(format!(
        "ai-memory-t0-orchestrate-envelope-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&out_dir).expect("create temp dir");
    let out_path = out_dir.join("plan.json");

    let status = Command::new(bin)
        .arg("--dry-run")
        .arg("--out")
        .arg(&out_path)
        .status()
        .expect("spawn t0-orchestrate --dry-run --out");
    assert!(status.success(), "E1: --dry-run --out exited non-zero");

    let body = std::fs::read_to_string(&out_path).expect("read envelope");
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("envelope is valid JSON");

    assert_eq!(parsed["mode"], "dry-run");
    assert!(parsed["plan"].is_array(), "E1: envelope missing plan array");
    let plan = parsed["plan"].as_array().expect("plan array");
    assert_eq!(
        plan.len(),
        24,
        "E1: expected 24 plan entries (4 LLMs × 6 questions), got {}",
        plan.len()
    );
    for entry in plan {
        for field in &[
            "llm", "model", "api_url", "auth_env", "qid", "profile", "question",
        ] {
            assert!(
                entry.get(field).is_some(),
                "E1: plan entry missing field `{field}`: {entry}"
            );
        }
    }
}
