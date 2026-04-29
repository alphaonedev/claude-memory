// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// Shared helpers for v0.6.3 integration tests.
//
// Charter §"Files to create" line 344 calls for a `tests/v063/` directory
// housing the new integration test suite for the v0.6.3 grand-slam Pillars
// (hierarchy, KG, duplicate-check). This module is the common harness:
// each top-level `tests/v063_*.rs` file is its own integration test binary
// and pulls these helpers in via `#[path = "v063/mod.rs"] mod v063;`.
//
// Tests should construct a fresh disposable SQLite DB per case using
// `tmp_db()` and exercise the binary through the same CLI / MCP surface
// that real callers use, so the suite is a faithful end-to-end check
// rather than an internals test.

#![allow(dead_code)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Build a `Command` for the compiled `ai-memory` CLI binary with the
/// `AI_MEMORY_NO_CONFIG=1` guard already wired in. Mirrors the same guard
/// the legacy `tests/integration.rs` uses so the two suites behave
/// identically under `cargo test`.
pub fn cmd() -> Command {
    let binary = env!("CARGO_BIN_EXE_ai-memory");
    let mut c = Command::new(binary);
    c.env("AI_MEMORY_NO_CONFIG", "1");
    c
}

/// Allocate a per-test `SQLite` path under the OS temp dir, prefixed
/// `ai-memory-v063-<scope>-<uuid>.db`. Each test should delete this on
/// its way out (best-effort).
pub fn tmp_db(scope: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "ai-memory-v063-{scope}-{}.db",
        uuid::Uuid::new_v4()
    ))
}

/// Spawn `ai-memory mcp` against `db`, write each line of `requests` to
/// stdin in order, close stdin, and return the captured stdout split on
/// newlines. Each request is one JSON-RPC frame; responses come back in
/// matching order.
pub fn mcp_exchange(db: &std::path::Path, requests: &[&str]) -> Vec<String> {
    let mut child = cmd()
        .args(["--db", db.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn mcp");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin");
        for req in requests {
            writeln!(stdin, "{req}").expect("write mcp request");
        }
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("mcp wait");
    assert!(
        output.status.success(),
        "mcp exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout)
        .trim()
        .lines()
        .map(str::to_owned)
        .collect()
}
