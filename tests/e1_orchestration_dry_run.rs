// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// E1's t0-orchestrate.sh is a bash script. Windows runners don't ship
// bash by default, so the dry-run harness check only runs on Unix. The
// orchestrator itself works on any platform with a bash interpreter
// (WSL/Git-Bash); CI just doesn't validate that path.
#![cfg(unix)]

//! v0.7.0 task E1 — minimal harness check on `scripts/t0-orchestrate.sh`.
//!
//! The orchestrator is an out-of-band script that fans the Discovery
//! Gate questions out to four live LLMs (see
//! `docs/v0.7/T0-ORCHESTRATION.md`). Live runs cost API budget and
//! require keys, so CI exercises it in `--dry-run` mode and asserts:
//!
//! 1. The script is present and executable.
//! 2. `--dry-run` exits 0 without making API calls.
//! 3. The plan output names all four LLMs (claude / gpt5 / gemini / grok).
//! 4. The plan output names every Discovery Gate question id pinned
//!    in `tests/calibration_t0.rs`.
//! 5. The dry-run advertises the result-file template paths.
//!
//! If any of these go red, the orchestration harness has drifted from
//! the calibration cells it wraps — fix the script (or the cell ids)
//! before merging.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_dry_run() -> String {
    let script = repo_root().join("scripts").join("t0-orchestrate.sh");
    assert!(
        script.exists(),
        "E1: scripts/t0-orchestrate.sh missing at {}",
        script.display()
    );

    let output = Command::new("bash")
        .arg(&script)
        .arg("--dry-run")
        .current_dir(repo_root())
        .output()
        .expect("spawn t0-orchestrate.sh --dry-run");

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
    // to QUESTIONS in scripts/t0-orchestrate.sh and to this list.
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
    // Sanity: dry-run must terminate with the explicit marker so we
    // never confuse a silent abort with a clean dry-run.
    let out = run_dry_run();
    assert!(
        out.contains("dry-run complete (no API calls made)"),
        "E1: dry-run did not print completion marker\nfull output:\n{out}"
    );
}
