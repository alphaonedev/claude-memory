// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7 B3-fix regression — `bootstrap_serve` must not block HTTP
//! `/health` on the family-descriptor embedding precompute.
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
//! The fix moves the precompute to a detached `tokio::spawn` task
//! whose result lands in `AppState::family_embeddings`
//! (`Arc<RwLock<Option<…>>>`) when ready. This test boots `ai-memory
//! serve` in the same way `DaemonGuard::spawn` does and asserts the
//! `/health` endpoint responds within **2 s** — well under the
//! original 5 s budget — proving the precompute is no longer on the
//! serve startup path.
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

    // 2-second budget: well under the 5 s `wait_for_health` budget
    // in `tests/integration.rs` that the original CI regression
    // overran. If the precompute ever creeps back onto the serve
    // startup path, this fails first.
    let result = wait_for_health_within(port, Duration::from_secs(2));

    // Always reap the child before asserting so a failed assertion
    // doesn't leave a zombie daemon holding the port.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&db);

    assert!(
        result.is_some(),
        "/api/v1/health did not respond within 2 s — \
         precompute_family_embeddings likely back on the serve \
         startup path (PR #592 B3-fix regression)",
    );
    let elapsed = result.unwrap();
    eprintln!("b3_precompute_does_not_block_serve_health: /health responded in {elapsed:?}");
}
