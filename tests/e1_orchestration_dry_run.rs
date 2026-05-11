// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 task E1 — minimal harness check on the `ai-memory-t0`
//! cross-platform orchestrator binary.
//!
//! Originally `scripts/t0-orchestrate.sh` was a bash script, so this
//! test was gated `#![cfg(unix)]` to keep Windows CI green. The
//! orchestrator now lives in `tools/t0-orchestrate/` as a standalone
//! Rust crate — the Unix gate is gone and the dry-run harness check
//! runs on every platform CI covers.
//!
//! The orchestrator fans the Discovery Gate questions out to four
//! live LLMs (see `docs/v0.7/T0-ORCHESTRATION.md`). Live runs cost
//! API budget and require keys, so CI exercises it in `--dry-run`
//! mode and asserts:
//!
//! 1. `--dry-run` exits 0 without making API calls.
//! 2. The plan output names all four LLMs (claude / gpt5 / gemini / grok).
//! 3. The plan output names every Discovery Gate question id pinned
//!    in `tests/calibration_t0.rs`.
//! 4. The dry-run advertises the result-file template paths.
//!
//! If any of these go red, the orchestration harness has drifted from
//! the calibration cells it wraps — fix the binary (or the cell ids)
//! before merging.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Build the orchestrator binary once per `cargo test` run and
/// return the absolute path to it.
///
/// Historically (when this was a bash script) each test fn invoked
/// `bash scripts/t0-orchestrate.sh` directly. With the Rust port,
/// `--test-threads > 1` would otherwise race parallel `cargo build`
/// invocations against the shared `--target-dir`. A process-wide
/// `OnceLock` lets the first thread to reach `orchestrator_bin()`
/// build the binary; every other thread blocks, then re-uses the
/// cached `PathBuf`. Same pattern `tests/g11_auto_link_detector.rs`
/// and `tests/transcript_extractor.rs` use for their sibling crates.
fn orchestrator_bin() -> &'static PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(build_orchestrator_once)
}

fn build_orchestrator_once() -> PathBuf {
    let manifest_path = repo_root().join("tools/t0-orchestrate/Cargo.toml");
    assert!(
        manifest_path.exists(),
        "E1: orchestrator manifest missing at {}",
        manifest_path.display()
    );

    // Per-test target dir scoped by PID so two concurrent `cargo
    // test` driver processes (e.g. CI sharding) cannot stomp each
    // other's target/.
    let target_dir = std::env::temp_dir().join(format!(
        "ai-memory-t0-orchestrate-target-{}",
        std::process::id()
    ));

    let status = Command::new("cargo")
        .args([
            "build",
            "--quiet",
            "--release",
            "--manifest-path",
            manifest_path.to_str().expect("utf-8 manifest path"),
            "--target-dir",
            target_dir.to_str().expect("utf-8 target dir"),
        ])
        .status()
        .expect("invoke cargo build for ai-memory-t0");
    assert!(status.success(), "cargo build for ai-memory-t0 failed");

    let bin = target_dir.join("release").join(if cfg!(windows) {
        "ai-memory-t0.exe"
    } else {
        "ai-memory-t0"
    });
    assert!(
        bin.exists(),
        "E1: ai-memory-t0 binary missing at {}",
        bin.display()
    );
    bin
}

fn run_dry_run() -> String {
    let bin = orchestrator_bin();
    let output = Command::new(bin)
        .arg("--dry-run")
        .current_dir(repo_root())
        .output()
        .expect("spawn ai-memory-t0 --dry-run");

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
    // tests/calibration_t0.rs. If a new cell lands there, add the id
    // to QUESTIONS in tools/t0-orchestrate/src/main.rs and to this list.
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
        out.contains("results/t0/") || out.contains("results\\t0\\"),
        "E1: dry-run results path should sit under results/t0/\nfull output:\n{out}"
    );
}

#[test]
fn e1_dry_run_makes_no_api_calls() {
    // Sanity: dry-run must terminate with the explicit marker so we
    // never confuse a silent abort with a clean dry-run.
    let out = run_dry_run();
    assert!(
        out.contains("dry-run complete (no API calls made)"),
        "E1: dry-run did not print completion marker\nfull output:\n{out}"
    );
}
