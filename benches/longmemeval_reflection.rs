// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// Bench harness scaffolding — pedantic relaxations that carry no
// behavioural meaning. Each is justified at its declaration site.
#![allow(
    // Module docstring + CLI-help text reference LLM, JSON, RFC3339 —
    // running clippy::doc_markdown over them adds noise without
    // catching anything load-bearing.
    clippy::doc_markdown,
    // Bench wrapper allocates owned `String` summaries that flow into
    // stdout / file writes; the per-call cost is negligible compared
    // with the per-scenario reflection passes.
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

//! v0.7.0 Layer 3 Task L3-1 — LongMemEval-Reflection benchmark
//! harness.
//!
//! The dataset + runner live under `benchmarks/longmemeval_reflection/`
//! (alongside the existing `benchmarks/longmemeval/` Python harness).
//! Cargo's `[[bench]]` directive requires the bench entrypoint to sit
//! under `benches/`, so this file is a thin wrapper that pulls in the
//! two modules with `#[path]` and drives them from `main()`.
//!
//! ## Modes
//!
//! ```bash
//! # CI smoke (≤6 scenarios, deterministic stub, ~3s on dev laptop):
//! cargo bench --bench longmemeval_reflection -- --test
//!
//! # Full CI run (all 50 scenarios, deterministic stub, ~30s):
//! cargo bench --bench longmemeval_reflection
//!
//! # Regenerate the on-disk dataset snapshot (used after a seed bump):
//! cargo bench --bench longmemeval_reflection -- --regenerate
//! ```
//!
//! The bench writes `target/bench/longmemeval-reflection.json` +
//! `target/bench/longmemeval-reflection.md` next to the existing
//! cross-harness bench artefacts and prints the markdown summary to
//! stdout. Non-zero exit when any spec gate fails (issue #674).

use std::path::PathBuf;

#[path = "../benchmarks/longmemeval_reflection/dataset.rs"]
mod dataset;

#[path = "../benchmarks/longmemeval_reflection/runner.rs"]
mod runner;

fn main() -> anyhow::Result<()> {
    // Argv parsing kept inline so the bench has no clap dep.
    // Recognised flags:
    //   --test          smoke run (≤ SMOKE_SCENARIO_LIMIT scenarios)
    //   --regenerate    re-materialise data/scenarios.jsonl and exit
    //   --load-snapshot read the committed snapshot instead of
    //                   regenerating in-memory (audit-replay path)
    let args: Vec<String> = std::env::args().collect();
    let smoke = args.iter().any(|a| a == "--test");
    let regenerate = args.iter().any(|a| a == "--regenerate");
    let load_snapshot = args.iter().any(|a| a == "--load-snapshot");

    // Resolve repo root so `target/bench/...` and the snapshot path
    // are stable regardless of cwd. `CARGO_MANIFEST_DIR` points at
    // the directory containing `Cargo.toml` — i.e. the crate root.
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let snapshot_path = repo_root
        .join("benchmarks")
        .join("longmemeval_reflection")
        .join("data")
        .join("scenarios.jsonl");

    if regenerate {
        let scenarios = dataset::generate_scenarios();
        let jsonl = dataset::serialise_jsonl(&scenarios);
        std::fs::create_dir_all(snapshot_path.parent().expect("snapshot parent"))?;
        std::fs::write(&snapshot_path, jsonl)?;
        println!(
            "regenerated {} scenarios → {}",
            scenarios.len(),
            snapshot_path.display()
        );
        return Ok(());
    }

    let scenarios = if load_snapshot {
        let jsonl = std::fs::read_to_string(&snapshot_path).map_err(|e| {
            anyhow::anyhow!(
                "scenarios.jsonl not found at {} — run with --regenerate first ({})",
                snapshot_path.display(),
                e
            )
        })?;
        dataset::load_jsonl(&jsonl)?
    } else {
        dataset::generate_scenarios()
    };

    let llm = runner::DeterministicLlmStub::from_scenarios(&scenarios);
    let judge = runner::DeterministicJudge::default();
    let report = runner::run(&scenarios, &llm, &judge, smoke)?;

    // Emit summary + JSON under target/bench (matches the cross-
    // harness bench convention).
    let bench_dir = repo_root.join("target").join("bench");
    std::fs::create_dir_all(&bench_dir)?;
    let json_path = bench_dir.join("longmemeval-reflection.json");
    let md_path = bench_dir.join("longmemeval-reflection.md");
    std::fs::write(&json_path, serde_json::to_string_pretty(&report)?)?;
    let md = report.render_markdown();
    std::fs::write(&md_path, &md)?;

    println!("{md}");
    println!("results: {}", json_path.display());

    match report.check_targets() {
        Ok(()) => {
            println!("ALL GATES PASS");
            Ok(())
        }
        Err(fails) => {
            // Fail loudly. Criterion's normal exit-code contract on
            // a custom-harness bench is: non-zero exit = bench
            // failure, surfaced through `cargo bench`. Mirrors what
            // the existing harness_bench / age_vs_cte benches do.
            for f in &fails {
                eprintln!("GATE FAIL: {f}");
            }
            anyhow::bail!("{} spec gate(s) failed", fails.len())
        }
    }
}
