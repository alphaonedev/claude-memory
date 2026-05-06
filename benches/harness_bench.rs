// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 D1 — cross-harness latency benchmark.
//!
//! Spawns `ai-memory mcp --profile full --tier keyword` once per harness
//! variant, performs a JSON-RPC `initialize` handshake with a different
//! `clientInfo.name` for each, and then measures end-to-end stdio
//! latency (write request line -> read response line) for the two
//! always-on bootstrap tools the Track D compat matrix calls out:
//!
//!   - `memory_capabilities` — the discovery cell every harness hits
//!     to learn what families/tools are visible.
//!   - `memory_recall`       — the load-bearing read path. The spec
//!     names `memory_load_family` (Track B1) as the second target,
//!     but B1 has not landed in `main` yet (no `"memory_load_family"`
//!     dispatch arm in `src/mcp.rs` as of this commit), so we fall
//!     back to `memory_recall` per the D1 spec's explicit fallback
//!     clause.
//!
//! The four harnesses simulated are the v0.7 first-class set from
//! `docs/v0.7/compatibility-matrix.html` + the catch-all generic case:
//!
//!   - `claude-code` (deferred-tool registration)
//!   - `cursor` (eager-load)
//!   - `vscode-anthropic` (treated as Generic by `Harness::detect`;
//!     stands in for the VS Code Anthropic extension that wasn't
//!     carved out as its own variant in B4)
//!   - `unknown` (the generic fallback)
//!
//! For each (harness, tool) pair we run `N=200` iterations after a
//! `WARMUP=10` warm-up burst, compute p50/p95/p99 in microseconds, and:
//!
//!   - Write a structured JSON report to
//!     `target/bench/harness-cross.json`.
//!   - Print a markdown table to stdout (so CI logs carry the table
//!     even when the JSON artifact isn't uploaded).
//!   - Fail the process if any p95 exceeds `P95_BUDGET_MS = 200ms`
//!     (loose budget; the strict 50ms recall p95 is enforced by
//!     `benches/recall.rs`, which this bench does NOT replace).
//!
//! This file uses `harness = false` and an ad-hoc main rather than
//! Criterion because we need (a) per-harness tagged percentiles in a
//! single artifact, and (b) the regression-gate behaviour at process
//! exit. Criterion's `bench_function` only emits per-bench reports, not
//! a single cross-cutting JSON document.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const N: usize = 200;
const WARMUP: usize = 10;
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// Loose p95 budget. The strict 50ms recall p95 lives in
/// `benches/recall.rs`. This gate exists to catch a 10x regression in
/// any harness's hot path before release, not to enforce the headline
/// SLO.
const P95_BUDGET_MS: u128 = 200;

/// The four harnesses the D1 spec names. `vscode-anthropic` is not a
/// distinct `Harness` variant (it falls through to `Generic`) but the
/// spec calls it out by name so we exercise it explicitly to detect
/// any future regression where the substrate special-cases it.
const HARNESSES: &[(&str, &str)] = &[
    ("claude-code", "claude-code"),
    ("cursor", "cursor"),
    ("vscode-anthropic", "vscode-anthropic"),
    ("generic", "unknown"),
];

/// Build the release binary once and return its path. Mirrors the
/// helper in `benches/recall.rs` so both benches resolve the binary
/// the same way (cargo metadata + release target dir).
fn binary_path() -> PathBuf {
    let output = Command::new("cargo")
        .args(["build", "--release"])
        .output()
        .expect("failed to build binary");
    assert!(
        output.status.success(),
        "cargo build --release failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to get cargo metadata");
    let meta: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let target_dir = meta["target_directory"].as_str().unwrap().to_string();
    PathBuf::from(format!("{target_dir}/release/ai-memory"))
}

/// RAII guard for a spawned `ai-memory mcp` child. Closing stdin lets
/// the server's read loop exit cleanly; if it doesn't, we kill it.
struct McpChild {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Drop for McpChild {
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Spawn `ai-memory mcp --profile full --tier keyword` against a
/// dedicated temp DB. Returns the child guard plus a receiver that the
/// stdout reader thread feeds line-by-line. Mirrors
/// `tests/mcp_integration.rs::spawn_mcp` so the bench and the
/// regression test agree on framing.
fn spawn_mcp(binary: &PathBuf, db_path: &str) -> (McpChild, mpsc::Receiver<String>) {
    let mut child = Command::new(binary)
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args([
            "--db",
            db_path,
            "mcp",
            "--profile",
            "full",
            "--tier",
            "keyword",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ai-memory mcp");

    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    // Drain stderr so the child never blocks on a full pipe.
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut s = stderr;
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
            }
        });
    }

    let (tx, rx) = mpsc::channel();
    spawn_stdout_reader(stdout, tx);
    (
        McpChild {
            child: Some(child),
            stdin: Some(stdin),
        },
        rx,
    )
}

fn spawn_stdout_reader(stdout: ChildStdout, tx: mpsc::Sender<String>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) if !line.trim().is_empty() => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

/// Send one JSON-RPC line and wait for the next response line. Returns
/// the parsed response so callers can sanity-check the result without
/// adding latency to the measurement (the timing we record is just the
/// write+read pair).
fn send_and_recv(
    stdin: &mut ChildStdin,
    rx: &mpsc::Receiver<String>,
    payload: &serde_json::Value,
) -> serde_json::Value {
    let line = serde_json::to_string(payload).unwrap();
    writeln!(stdin, "{line}").expect("write to mcp stdin");
    stdin.flush().expect("flush mcp stdin");
    let resp = rx
        .recv_timeout(READ_TIMEOUT)
        .expect("mcp response did not arrive within READ_TIMEOUT");
    serde_json::from_str(&resp).unwrap_or_else(|e| panic!("parse mcp response: {e}: {resp}"))
}

/// Drive `initialize` with the supplied `clientInfo.name`. Returns once
/// the handshake is acknowledged so subsequent `tools/call` measurements
/// don't include the one-time setup cost.
fn initialize(stdin: &mut ChildStdin, rx: &mpsc::Receiver<String>, client_name: &str) {
    let resp = send_and_recv(
        stdin,
        rx,
        &serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {"name": client_name, "version": "bench"}
            }
        }),
    );
    assert_eq!(resp["jsonrpc"], "2.0", "initialize handshake malformed");
    assert!(
        resp["result"].is_object(),
        "initialize did not return result for clientInfo.name={client_name}: {resp}"
    );
}

/// Ascending-sorted `samples`, return the value at the given percentile
/// (0..=100) using nearest-rank. Panics on empty input — every call
/// site guarantees at least N samples by construction.
fn percentile(sorted: &[u128], pct: f64) -> u128 {
    assert!(!sorted.is_empty(), "percentile of empty sample set");
    // Bench-internal helper: sample sets are bounded by the
    // benchmark `iters` argument (≤ a few thousand), well within
    // f64 mantissa precision and never negative or fractional.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let rank = ((pct / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

/// Time `iters` iterations of `op` and return all observed latencies in
/// microseconds. A `WARMUP` burst runs first and is discarded so the
/// first-call JIT-y costs (page-cache miss on the `SQLite` file, BPE
/// table init, etc.) don't skew the percentiles.
fn measure<F>(iters: usize, mut op: F) -> Vec<u128>
where
    F: FnMut(),
{
    for _ in 0..WARMUP {
        op();
    }
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let start = Instant::now();
        op();
        samples.push(start.elapsed().as_micros());
    }
    samples
}

/// One row of the cross-harness report.
#[derive(serde::Serialize)]
struct Row {
    harness: String,
    client_info_name: String,
    tool: String,
    iterations: usize,
    p50_us: u128,
    p95_us: u128,
    p99_us: u128,
    min_us: u128,
    max_us: u128,
}

#[allow(clippy::too_many_lines)] // bench driver; sequential JSON-emit pipeline
fn main() {
    let binary = binary_path();
    eprintln!("[harness_bench] binary = {}", binary.display());

    let mut rows: Vec<Row> = Vec::with_capacity(HARNESSES.len() * 2);

    for (label, client_name) in HARNESSES {
        eprintln!("[harness_bench] harness = {label} (clientInfo.name = {client_name})");
        let dir = std::env::temp_dir();
        let db_path = dir
            .join(format!(
                "ai-memory-bench-harness-{}-{}.db",
                label,
                uuid::Uuid::new_v4()
            ))
            .to_str()
            .unwrap()
            .to_string();

        let (mut guard, rx) = spawn_mcp(&binary, &db_path);
        let stdin = guard.stdin.as_mut().unwrap();
        initialize(stdin, &rx, client_name);

        // ---- memory_capabilities ----
        let cap_call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {"name": "memory_capabilities", "arguments": {}}
        });
        let mut id: u64 = 100;
        let mut cap_samples = measure(N, || {
            id += 1;
            let mut req = cap_call.clone();
            req["id"] = serde_json::json!(id);
            let resp = send_and_recv(stdin, &rx, &req);
            assert_eq!(resp["id"], serde_json::json!(id));
        });
        cap_samples.sort_unstable();
        rows.push(Row {
            harness: (*label).to_string(),
            client_info_name: (*client_name).to_string(),
            tool: "memory_capabilities".to_string(),
            iterations: N,
            p50_us: percentile(&cap_samples, 50.0),
            p95_us: percentile(&cap_samples, 95.0),
            p99_us: percentile(&cap_samples, 99.0),
            min_us: *cap_samples.first().unwrap(),
            max_us: *cap_samples.last().unwrap(),
        });

        // ---- memory_recall (stand-in for B1 memory_load_family) ----
        // Seed a single memory so recall has something to scan; we
        // intentionally do NOT seed 1k rows — recall.rs already covers
        // the loaded-DB case. This bench's job is harness-level
        // overhead, not query-engine perf.
        let _ = send_and_recv(
            stdin,
            &rx,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": "memory_store",
                    "arguments": {
                        "title": "harness-bench-seed",
                        "content": "harness benchmark seed content for recall path",
                        "tier": "mid",
                        "namespace": "harness-bench"
                    }
                }
            }),
        );

        let recall_call = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 200,
            "method": "tools/call",
            "params": {
                "name": "memory_recall",
                "arguments": {
                    "context": "harness benchmark seed",
                    "namespace": "harness-bench"
                }
            }
        });
        let mut rid: u64 = 200;
        let mut recall_samples = measure(N, || {
            rid += 1;
            let mut req = recall_call.clone();
            req["id"] = serde_json::json!(rid);
            let resp = send_and_recv(stdin, &rx, &req);
            assert_eq!(resp["id"], serde_json::json!(rid));
        });
        recall_samples.sort_unstable();
        rows.push(Row {
            harness: (*label).to_string(),
            client_info_name: (*client_name).to_string(),
            tool: "memory_recall".to_string(),
            iterations: N,
            p50_us: percentile(&recall_samples, 50.0),
            p95_us: percentile(&recall_samples, 95.0),
            p99_us: percentile(&recall_samples, 99.0),
            min_us: *recall_samples.first().unwrap(),
            max_us: *recall_samples.last().unwrap(),
        });

        drop(guard); // shut the child down before the next iteration
        let _ = std::fs::remove_file(&db_path);
    }

    // ---- markdown table to stdout ----
    println!("# v0.7 D1 cross-harness latency report\n");
    println!(
        "| harness | clientInfo.name | tool | iters | p50 (ms) | p95 (ms) | p99 (ms) | min (ms) | max (ms) |"
    );
    println!("|---|---|---|---:|---:|---:|---:|---:|---:|");
    // Display-only conversion: latencies are bench durations measured in
    // microseconds (typically <10s, well within f64 mantissa precision).
    #[allow(clippy::cast_precision_loss)]
    for r in &rows {
        println!(
            "| {} | {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} |",
            r.harness,
            r.client_info_name,
            r.tool,
            r.iterations,
            r.p50_us as f64 / 1000.0,
            r.p95_us as f64 / 1000.0,
            r.p99_us as f64 / 1000.0,
            r.min_us as f64 / 1000.0,
            r.max_us as f64 / 1000.0,
        );
    }
    println!();

    // ---- structured JSON artifact ----
    let out_dir = PathBuf::from("target/bench");
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        eprintln!(
            "[harness_bench] failed to create {}: {e}",
            out_dir.display()
        );
    }
    let report = serde_json::json!({
        "schema": "ai-memory.bench.harness-cross.v1",
        "version": env!("CARGO_PKG_VERSION"),
        "iterations": N,
        "warmup": WARMUP,
        "p95_budget_ms": P95_BUDGET_MS,
        "rows": &rows,
    });
    let out_file = out_dir.join("harness-cross.json");
    match std::fs::write(&out_file, serde_json::to_string_pretty(&report).unwrap()) {
        Ok(()) => eprintln!("[harness_bench] wrote {}", out_file.display()),
        Err(e) => eprintln!(
            "[harness_bench] failed to write {}: {e}",
            out_file.display()
        ),
    }

    // ---- regression gate ----
    let mut violations: Vec<String> = Vec::new();
    for r in &rows {
        let p95_ms = r.p95_us / 1000;
        if p95_ms > P95_BUDGET_MS {
            violations.push(format!(
                "{}.{} p95 = {}ms > {}ms",
                r.harness, r.tool, p95_ms, P95_BUDGET_MS
            ));
        }
    }
    if !violations.is_empty() {
        eprintln!(
            "[harness_bench] FAIL: {} p95 budget violation(s):",
            violations.len()
        );
        for v in &violations {
            eprintln!("  - {v}");
        }
        std::process::exit(1);
    }
    eprintln!("[harness_bench] OK — all harnesses within {P95_BUDGET_MS}ms p95 budget");
}
