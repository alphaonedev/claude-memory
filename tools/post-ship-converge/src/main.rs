// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7.0.1 — closes #625.
//
// Cross-platform Rust port of `scripts/post-ship-converge.sh`
// (v0.7.0 task E2). PR #622 (the original bash version) had not
// merged when this Rust port landed, so this crate stands on its
// own as the canonical implementation — the bash script is never
// added to the tree.
//
// Goal: after a release tag is cut, verify all three distribution
// channels (`cargo install`, `brew install`, prebuilt binary
// tarball) advertise the *same* version string. A divergence
// means a partial publish (cargo accepted, brew formula not
// updated, GitHub release artifacts missing, etc.) and blocks
// post-ship comms until reconciled.
//
// The orchestrator runs in two modes:
//
//   --dry-run     prints the convergence plan + JSON envelope
//                 without touching the network. CI exercises this
//                 mode on every PR via
//                 `tests/e2_post_ship_dry_run.rs`.
//
//   live (default) hits each channel's metadata endpoint and
//                 collates the observed version per channel.
//                 Requires `--version <VER>` so we know what to
//                 compare against.
//
// `--method <cargo|brew|binary>` restricts a live run to a single
// channel — useful when one publish step lagged and you want to
// poll just that channel until convergence.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Distribution channels — (id, name, metadata-url-template). The
// `{version}` placeholder is filled in per run; channels that
// don't depend on the version string still receive it but ignore
// it.
// ---------------------------------------------------------------------------
struct Channel {
    id: &'static str,
    name: &'static str,
    metadata_url: &'static str,
}

const CHANNELS: &[Channel] = &[
    Channel {
        id: "cargo",
        name: "crates.io",
        metadata_url: "https://crates.io/api/v1/crates/ai-memory",
    },
    Channel {
        id: "brew",
        name: "Homebrew",
        metadata_url: "https://raw.githubusercontent.com/alphaonedev/homebrew-tap/main/Formula/ai-memory.rb",
    },
    Channel {
        id: "binary",
        name: "GitHub release tarball",
        metadata_url: "https://api.github.com/repos/alphaonedev/ai-memory-mcp/releases/tags/v{version}",
    },
];

#[derive(Copy, Clone, Debug, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
enum Method {
    Cargo,
    Brew,
    Binary,
}

impl Method {
    const fn id(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Brew => "brew",
            Self::Binary => "binary",
        }
    }
}

// ---------------------------------------------------------------------------
// CLI surface.
// ---------------------------------------------------------------------------
#[derive(Parser, Debug)]
#[command(
    name = "post-ship-converge",
    about = "v0.7 E2 post-ship distribution-channel convergence verifier",
    long_about = "Verifies that a freshly-cut ai-memory release advertises \
                  the same version string across cargo / brew / GitHub \
                  release tarball channels. Run after every tag cut."
)]
struct Cli {
    /// Print the convergence plan + JSON envelope without
    /// hitting any network endpoints.
    #[arg(long)]
    dry_run: bool,

    /// Version under verification. Required for live runs;
    /// optional for `--dry-run` (defaults to a placeholder so
    /// the dry-run plan is still meaningful).
    #[arg(long, value_name = "VER")]
    version: Option<String>,

    /// Restrict the run to a single distribution channel.
    /// Default: probe all three.
    #[arg(long, value_enum)]
    method: Option<Method>,

    /// Path to write the JSON envelope to. In `--dry-run` this is
    /// the convergence plan; in live mode it's the
    /// observed-vs-expected report. When omitted in live mode,
    /// falls back to `results/post-ship/converge-<ts>.json`.
    #[arg(long, value_name = "PATH")]
    out: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// JSON envelope shape — emitted to `--out` (when set) and a copy
// printed to stdout under `--dry-run`. Field names are stable:
// `tests/e2_post_ship_dry_run.rs` asserts on them.
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize, Debug)]
struct PlanEntry {
    method: String,
    channel: String,
    metadata_url: String,
    expected_version: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct DryRunEnvelope {
    mode: String,
    timestamp: String,
    expected_version: String,
    only_method: Option<String>,
    channels: usize,
    results_template: String,
    plan: Vec<PlanEntry>,
}

fn timestamp_utc() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

fn build_plan(only: Option<Method>, version: &str) -> Vec<PlanEntry> {
    let only_id = only.map(Method::id);
    CHANNELS
        .iter()
        .filter(|c| only_id.is_none_or(|id| c.id == id))
        .map(|c| PlanEntry {
            method: c.id.to_string(),
            channel: c.name.to_string(),
            metadata_url: c.metadata_url.replace("{version}", version),
            expected_version: version.to_string(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Dry-run output — the integration test substring-matches on the
// `method:` lines plus each channel id, so the layout below is
// load-bearing.
// ---------------------------------------------------------------------------
fn run_dry(cli: &Cli) -> std::io::Result<()> {
    let timestamp = timestamp_utc();
    let version = cli
        .version
        .clone()
        .unwrap_or_else(|| "0.0.0-dry-run".to_string());
    let plan = build_plan(cli.method, &version);
    let results_template = format!("results/post-ship/converge-{timestamp}.json");

    println!("==> post-ship-converge");
    println!("    timestamp:  {timestamp}");
    println!("    dry-run:    1");
    println!("    version:    {version}");
    println!(
        "    only-method: {}",
        cli.method.map_or("<all>", Method::id)
    );
    println!("    channels:   {}", plan.len());
    println!();
    println!("plan:");
    for entry in &plan {
        // Same column-aligned shape `t0-orchestrate` uses so the
        // integration test layer can substring-match
        // `method:   <id>` deterministically.
        println!("  - method:   {}", entry.method);
        println!("    channel:  {}", entry.channel);
        println!("    url:      {}", entry.metadata_url);
        println!("    expected: {}", entry.expected_version);
    }
    println!();
    println!("results_template: {results_template}");

    let envelope = DryRunEnvelope {
        mode: "dry-run".to_string(),
        timestamp: timestamp.clone(),
        expected_version: version,
        only_method: cli.method.map(|m| m.id().to_string()),
        channels: plan.len(),
        results_template,
        plan,
    };

    if let Some(path) = cli.out.as_ref() {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(path)?;
        serde_json::to_writer_pretty(&mut f, &envelope)?;
        f.write_all(b"\n")?;
        println!("wrote: {}", path.display());
    }

    println!("==> dry-run complete (no network calls made)");
    Ok(())
}

// ---------------------------------------------------------------------------
// Live mode — fetch each channel's metadata endpoint, extract the
// version string, and report convergence vs. divergence.
// ---------------------------------------------------------------------------
fn extract_version(channel_id: &str, body_text: &str) -> Option<String> {
    match channel_id {
        "cargo" => serde_json::from_str::<Value>(body_text)
            .ok()?
            .get("crate")?
            .get("max_stable_version")
            .and_then(Value::as_str)
            .map(str::to_string),
        "brew" => {
            // Homebrew formula is Ruby. Pull the first
            // `version "X.Y.Z"` declaration with a tiny
            // hand-roll — avoids dragging a regex crate into
            // this CI helper.
            for line in body_text.lines() {
                let trimmed = line.trim();
                if let Some(rest) = trimmed.strip_prefix("version ")
                    && let Some(start) = rest.find('"')
                    && let Some(end) = rest[start + 1..].find('"')
                {
                    return Some(rest[start + 1..start + 1 + end].to_string());
                }
            }
            None
        }
        "binary" => serde_json::from_str::<Value>(body_text)
            .ok()?
            .get("tag_name")
            .and_then(Value::as_str)
            .map(|s| s.trim_start_matches('v').to_string()),
        _ => None,
    }
}

fn run_live(cli: &Cli) -> std::io::Result<()> {
    let Some(version) = cli.version.clone() else {
        eprintln!("post-ship-converge: --version is required in live mode");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing --version",
        ));
    };
    let timestamp = timestamp_utc();
    let plan = build_plan(cli.method, &version);

    println!("==> post-ship-converge");
    println!("    timestamp:  {timestamp}");
    println!("    dry-run:    0");
    println!("    version:    {version}");
    println!(
        "    only-method: {}",
        cli.method.map_or("<all>", Method::id)
    );
    println!("    channels:   {}", plan.len());
    println!();

    let client = reqwest::blocking::Client::builder()
        .user_agent("ai-memory-post-ship-converge/0.1")
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("build reqwest client");

    let mut observed: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut all_match = true;
    for entry in &plan {
        print!("    {} ({}): ", entry.method, entry.channel);
        let result = client.get(&entry.metadata_url).send();
        let observed_version = match result {
            Ok(resp) if resp.status().is_success() => resp
                .text()
                .ok()
                .and_then(|t| extract_version(&entry.method, &t)),
            Ok(resp) => {
                println!("HTTP {}", resp.status());
                observed.insert(entry.method.clone(), None);
                all_match = false;
                continue;
            }
            Err(e) => {
                println!("error: {e}");
                observed.insert(entry.method.clone(), None);
                all_match = false;
                continue;
            }
        };
        let matches = observed_version.as_deref() == Some(version.as_str());
        if !matches {
            all_match = false;
        }
        println!(
            "{} (expected {version}) [{}]",
            observed_version.as_deref().unwrap_or("<unknown>"),
            if matches { "ok" } else { "MISMATCH" }
        );
        observed.insert(entry.method.clone(), observed_version);
    }

    let report = json!({
        "mode": "live",
        "timestamp": timestamp,
        "expected_version": version,
        "all_converged": all_match,
        "observed": observed,
    });

    let out_path = cli.out.clone().unwrap_or_else(|| {
        let dir = PathBuf::from("results").join("post-ship");
        let _ = fs::create_dir_all(&dir);
        dir.join(format!("converge-{timestamp}.json"))
    });
    if let Some(parent) = out_path.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut f = fs::File::create(&out_path)?;
    serde_json::to_writer_pretty(&mut f, &report)?;
    f.write_all(b"\n")?;
    println!();
    println!("==> report: {}", out_path.display());

    if all_match {
        println!("==> all channels converged on v{version}");
        Ok(())
    } else {
        eprintln!("==> CONVERGENCE FAILED — see report for details");
        Err(std::io::Error::other("convergence mismatch"))
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = if cli.dry_run {
        run_dry(&cli)
    } else {
        run_live(&cli)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("post-ship-converge: {e}");
            ExitCode::FAILURE
        }
    }
}
