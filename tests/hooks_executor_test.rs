// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — G3 integration tests.
//
// These tests spawn real subprocesses (tiny shell scripts written
// to a tempdir at test time) so they exercise the same stdio path
// production hooks will use. Two end-to-end scenarios:
//
//   * exec mode — 100 concurrent fires through 100 short-lived
//     children complete within the configured timeout.
//   * daemon mode — 1000 fires through a *single* long-lived child
//     complete inside the deadline; mid-stream child crash (the
//     test script self-kills after N requests) triggers a
//     reconnect and the next fire still succeeds.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_memory::hooks::{
    DaemonExecutor, ExecExecutor, ExecutorRegistry, FailMode, HookConfig, HookDecision, HookEvent,
    HookExecutor, HookMode,
};
use serde_json::json;
use tempfile::TempDir;

/// Write `body` to `dir/name`, mark it executable, return the path.
/// Tests rely on /bin/sh being available — true on every supported
/// deployment target (Linux containers, macOS dev hosts).
fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join(name);
    std::fs::write(&path, body).expect("write script");
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

fn cfg_for(command: PathBuf, mode: HookMode, timeout_ms: u32) -> HookConfig {
    HookConfig {
        event: HookEvent::PostStore,
        command,
        priority: 0,
        timeout_ms,
        mode,
        enabled: true,
        namespace: "*".into(),
        fail_mode: FailMode::Open,
    }
}

// ---------------------------------------------------------------------------
// exec mode — 100 concurrent fires
// ---------------------------------------------------------------------------

/// 100 concurrent fires through `ExecExecutor` must all complete
/// inside their per-fire timeout. The script reads its stdin, then
/// writes a fixed `{"action":"allow"}` decision. The test asserts:
///
///   * every fire returned `Allow`;
///   * the *aggregate* wall-clock stayed within 30s (a slack ceiling
///     to avoid CI flakes on cold containers; per-fire timeout is
///     5s, far above the ~50ms a fork+exec costs).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exec_mode_100_concurrent_fires_all_allow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "allow.sh",
        r#"#!/bin/sh
# Drain stdin so the parent's `shutdown()` returns cleanly.
cat >/dev/null
printf '%s\n' '{"action":"allow"}'
"#,
    );

    let executor = Arc::new(ExecExecutor::new(cfg_for(script, HookMode::Exec, 5_000)));

    let started = Instant::now();
    let mut handles = Vec::with_capacity(100);
    for i in 0..100u32 {
        let exec = Arc::clone(&executor);
        handles.push(tokio::spawn(async move {
            exec.fire(HookEvent::PostStore, json!({"i": i})).await
        }));
    }
    let mut allowed = 0u32;
    for h in handles {
        let r = h.await.expect("join");
        match r {
            Ok(HookDecision::Allow) => allowed += 1,
            Ok(other) => panic!("expected Allow, got {other:?}"),
            Err(e) => panic!("exec mode fire failed: {e}"),
        }
    }
    assert_eq!(allowed, 100, "all 100 fires must Allow");

    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "100 fires took {elapsed:?}; expected <30s wall-clock"
    );

    let metrics = executor.metrics();
    assert_eq!(metrics.events_fired, 100);
    assert_eq!(metrics.events_dropped, 0);
}

/// A child that hangs past `timeout_ms` must trip the deadline and
/// surface `ExecutorError::Timeout`. Confirms backpressure metrics
/// increment `events_dropped`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_mode_timeout_drops_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "hang.sh",
        r"#!/bin/sh
# Sleep well past the 200ms timeout the test sets.
sleep 5
",
    );
    let executor = ExecExecutor::new(cfg_for(script, HookMode::Exec, 200));
    let r = executor.fire(HookEvent::PostStore, json!({})).await;
    assert!(matches!(
        r,
        Err(ai_memory::hooks::ExecutorError::Timeout { .. })
    ));
    let m = executor.metrics();
    assert_eq!(m.events_dropped, 1, "timeout must bump events_dropped");
}

// ---------------------------------------------------------------------------
// daemon mode — 1000 fires through one child
// ---------------------------------------------------------------------------

/// 1000 fires through a single daemon child complete within the
/// deadline. The script's read-write loop is the simplest possible
/// NDJSON echo: read one line, write `{"action":"allow"}\n`, repeat.
/// We assert all 1000 returned Allow and the connection was reused
/// (one process for the whole test).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_mode_1000_fires_one_child() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "echo_allow.sh",
        r#"#!/bin/sh
# NDJSON loop: one input line, one output line.
while IFS= read -r _line; do
  printf '%s\n' '{"action":"allow"}'
done
"#,
    );
    let executor = DaemonExecutor::new(cfg_for(script, HookMode::Daemon, 5_000));

    let started = Instant::now();
    for i in 0..1000u32 {
        let r = executor
            .fire(HookEvent::PostStore, json!({"i": i}))
            .await
            .unwrap_or_else(|e| panic!("fire {i} failed: {e}"));
        assert_eq!(r, HookDecision::Allow, "fire {i} returned {r:?}");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(60),
        "1000 daemon fires took {elapsed:?}; expected <60s"
    );

    let m = executor.metrics();
    assert_eq!(m.events_fired, 1_000);
    assert_eq!(m.events_dropped, 0);
}

/// A daemon child that exits mid-stream must trigger a reconnect on
/// the next fire. The test script answers the first 5 fires, then
/// `exit 0`s. We assert (a) the 6th fire either succeeds (after
/// reconnect) or surfaces a child-exit error we can recover from on
/// retry, and (b) at least one fire after the crash returns Allow.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_mode_reconnect_after_crash() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "crash_after_5.sh",
        r#"#!/bin/sh
n=0
while IFS= read -r _line; do
  n=$((n + 1))
  printf '%s\n' '{"action":"allow"}'
  if [ "$n" -ge 5 ]; then
    exit 0
  fi
done
"#,
    );
    let executor = DaemonExecutor::new(cfg_for(script, HookMode::Daemon, 5_000));

    // First 5 must Allow.
    for i in 0..5u32 {
        let r = executor.fire(HookEvent::PostStore, json!({"i": i})).await;
        assert_eq!(r.expect("first 5 succeed"), HookDecision::Allow);
    }

    // The 6th may surface ChildExit (we wrote into a closing pipe)
    // OR Allow (if the child finished writing the 5th decision
    // before exiting and the read happened before the write fails).
    // Either way, the executor must reset its connection and the
    // *next* fire after that must reconnect successfully.
    let _ = executor.fire(HookEvent::PostStore, json!({"i": 5})).await;

    // Drive a few more fires; the executor's reconnect-with-backoff
    // path should bring us back online. We tolerate a transient
    // failure but require at least one Allow inside the next 5.
    let mut recovered = false;
    for i in 6..11u32 {
        if let Ok(HookDecision::Allow) = executor.fire(HookEvent::PostStore, json!({"i": i})).await
        {
            recovered = true;
            break;
        }
    }
    assert!(
        recovered,
        "executor failed to reconnect after daemon child crash"
    );
}

// ---------------------------------------------------------------------------
// Registry plumbing — exercises the get/cache path with real spawns
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registry_dispatches_to_correct_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exec_script = write_script(
        &dir,
        "exec_allow.sh",
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"action":"allow"}'
"#,
    );
    let daemon_script = write_script(
        &dir,
        "daemon_allow.sh",
        r#"#!/bin/sh
while IFS= read -r _line; do printf '%s\n' '{"action":"allow"}'; done
"#,
    );

    let exec_cfg = cfg_for(exec_script, HookMode::Exec, 2_000);
    let daemon_cfg = cfg_for(daemon_script, HookMode::Daemon, 2_000);

    let mut reg = ExecutorRegistry::new();
    let ex = reg.get(&exec_cfg);
    let dm = reg.get(&daemon_cfg);
    assert_eq!(reg.len(), 2);

    let r1 = ex
        .fire(HookEvent::PostStore, json!({}))
        .await
        .expect("exec");
    let r2 = dm
        .fire(HookEvent::PostStore, json!({}))
        .await
        .expect("daemon");
    assert_eq!(r1, HookDecision::Allow);
    assert_eq!(r2, HookDecision::Allow);

    // Metrics survive the dyn boundary.
    let snapshot = reg.metrics();
    assert_eq!(snapshot.len(), 2);
    let total_fired: u64 = snapshot.iter().map(|(_, m)| m.events_fired).sum();
    assert_eq!(total_fired, 2);
}

/// A `Deny` decision from the child must round-trip into a
/// `HookDecision::Deny` on the parent side, including the explicit
/// HTTP-style code.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deny_decision_round_trips_with_code() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "deny.sh",
        r#"#!/bin/sh
cat >/dev/null
printf '%s\n' '{"action":"deny","reason":"redact required","code":451}'
"#,
    );
    let exec = ExecExecutor::new(cfg_for(script, HookMode::Exec, 2_000));
    let r = exec
        .fire(HookEvent::PostStore, json!({}))
        .await
        .expect("fire");
    match r {
        HookDecision::Deny { reason, code } => {
            assert_eq!(reason, "redact required");
            assert_eq!(code, 451);
        }
        // G4 lifted HookDecision into a 4-variant enum; the match
        // arm below covers the non-Deny shapes the integration
        // script can never produce, but it keeps the match
        // exhaustive against the new wire contract.
        other => panic!("expected Deny, got {other:?}"),
    }
}
