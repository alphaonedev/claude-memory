// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 B3-fix regression — `bootstrap_serve` must not block HTTP
//! `/health` on the family-descriptor embedding precompute, even
//! when the precompute is explicitly opted-in.
//!
//! ## What this guards against
//!
//! PR #592 (B3: family-descriptor embeddings) shipped a synchronous
//! call to `AppState::precompute_family_embeddings(embedder.as_ref())`
//! in `src/daemon_runtime.rs::bootstrap_serve` *before* the Axum
//! router was bound. On CI runners without a pre-warmed `hf-hub`
//! model cache the embedder's first `embed()` call triggered a
//! model download that blocked past the integration suite's
//! `wait_for_health` budget (50 × 100 ms = 5 s), causing ~30
//! integration tests to fail at `tests/integration.rs:8924` on
//! Linux, macOS, and Windows.
//!
//! v0.7 B3-fix2 then gated the precompute behind
//! `AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS=1` (default OFF) after a
//! follow-up CI failure pattern showed the *detached* spawn_blocking
//! variant still serialised request-path embeds on the embedder's
//! `std::sync::Mutex<BertModel>` under parallel test load — surfacing
//! as `http_notify_fans_out_…` 503 quorum failures and
//! `test_serve_mtls_…` POST timeouts that did not occur on
//! `origin/main`. This test sets the env var ON to exercise the
//! enabled-precompute path and proves `/health` still responds in
//! the integration-suite budget — so the day B2 wires the smart
//! loader and the gate flips on by default, the boot path is still
//! safe.
//!
//! Cross-platform: `assert_cmd` + `std::process::Command` only,
//! no shell, no Unix-only signals.

use assert_cmd::cargo::CommandCargoExt;
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Bind a port, drop the listener, return the now-free port. Mirrors
/// the `free_port` helper in `tests/integration.rs` so this test
/// behaves identically to the suite it protects.
fn free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

/// Poll `GET /api/v1/health` until it returns `200` or `budget`
/// elapses. Returns `Some(elapsed)` on success, `None` on timeout.
fn wait_for_health_within(port: u16, budget: Duration) -> Option<Duration> {
    let started = Instant::now();
    while started.elapsed() < budget {
        let out = Command::new("curl")
            .args([
                "-s",
                "-o",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
                "-w",
                "%{http_code}",
                "--max-time",
                "1",
                &format!("http://127.0.0.1:{port}/api/v1/health"),
            ])
            .output();
        if let Ok(out) = out
            && String::from_utf8_lossy(&out.stdout) == "200"
        {
            return Some(started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
fn b3_precompute_does_not_block_serve_health() {
    let dir = std::env::temp_dir();
    let db = dir.join(format!(
        "ai-memory-b3-precompute-block-{}.db",
        uuid::Uuid::new_v4()
    ));
    let port = free_port();

    // `AI_MEMORY_NO_CONFIG=1` mirrors the project's standard test env
    // (see CLAUDE.md): prevents loading user config that could
    // trigger embedder/LLM init outside the precompute we're
    // probing. The bug doesn't depend on this env var, but setting
    // it isolates the test from operator environment leaks.
    let mut child = Command::cargo_bin("ai-memory")
        .expect("locate ai-memory cargo bin")
        .env("AI_MEMORY_NO_CONFIG", "1")
        // `HF_HUB_OFFLINE=1` keeps the candle/hf-hub path from
        // attempting a network download in the spawned task. Even
        // when the precompute is on a background task, this guards
        // against the test itself making CI slow.
        .env("HF_HUB_OFFLINE", "1")
        // v0.7 B3-fix2 — the precompute is gated OFF by default
        // (see `bootstrap_serve` in `src/daemon_runtime.rs`). Flip
        // it ON here so the test actually exercises the precompute
        // path: a regression that re-blocks `/health` on the
        // precompute *under the explicit opt-in* would still fail
        // the integration suite the day B2 enables the gate.
        .env("AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS", "1")
        .args([
            "--db",
            db.to_str().unwrap(),
            "serve",
            "--port",
            &port.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn ai-memory serve");

    // 5-second budget matches `tests/integration.rs::wait_for_health`
    // (50 × 100 ms). A regression that re-couples `/health` to the
    // precompute (e.g. by `await`ing the precompute task before
    // returning from `bootstrap_serve`) would blow this budget on
    // the same CI runners that exposed the original bug. The looser
    // 5 s — vs the prior 2 s — bound was chosen after Windows
    // runners measured 2.34 s for the embedder *load* (separate
    // from the precompute) on cold-start; the original 2 s was
    // tight enough to false-positive on slow runners while not
    // catching anything 5 s does not.
    let result = wait_for_health_within(port, Duration::from_secs(5));

    // Always reap the child before asserting so a failed assertion
    // doesn't leave a zombie daemon holding the port.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&db);

    assert!(
        result.is_some(),
        "/api/v1/health did not respond within 5 s with \
         AI_MEMORY_PRECOMPUTE_FAMILY_EMBEDDINGS=1 — \
         precompute_family_embeddings likely back on the serve \
         startup path (PR #592 B3-fix regression)",
    );
    let elapsed = result.unwrap();
    eprintln!("b3_precompute_does_not_block_serve_health: /health responded in {elapsed:?}");
}
