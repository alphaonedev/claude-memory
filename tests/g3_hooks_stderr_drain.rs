// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//
// v0.7.0 review #628 blocker H9 — daemon-mode stderr never drained.
//
// Background. The G3 daemon executor reads stdout from the long-lived
// child but, prior to this fix, never drained stderr. A verbose hook
// (one that writes diagnostics on every fire) would fill the OS pipe
// buffer (~64 KiB on Linux, ~16 KiB on macOS) and the child would
// block on its next `write(2)` to stderr — which would in turn
// deadlock the executor on its next `read_line` from stdout, because
// the child can't service the request until it can finish flushing
// stderr. The fix spawns a per-child background task that drains
// stderr into a bounded ring buffer (last 4 KiB), so the pipe stays
// drained no matter how chatty the hook is, AND the operator log
// surfaces the buffered tail on timeout / failure so diagnostics
// aren't silently swallowed.
//
// This test exercises both halves of the fix end-to-end:
//
//   1. Spawn a daemon-mode hook whose script writes ~1 MiB of stderr
//      (well past every supported platform's pipe buffer) before
//      writing each NDJSON decision. Without the drain task this
//      would deadlock the second fire; with it, multiple fires must
//      complete inside the timeout.
//
//   2. After the fires complete, force a timeout on a follow-up fire
//      (the script enters a sleep loop after N fires) and assert the
//      executor surfaces `Timeout` cleanly without hanging — the
//      drain task must let the executor's `tokio::time::timeout`
//      trip on schedule rather than getting stuck on a full pipe.
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ai_memory::hooks::{
    DaemonExecutor, ExecExecutor, ExecutorError, FailMode, HookConfig, HookDecision, HookEvent,
    HookExecutor, HookMode,
};
use serde_json::json;
use tempfile::TempDir;

/// Write `body` to `dir/name`, mark it executable, return the path.
/// Same shape as the helper in `tests/hooks_executor_test.rs` — kept
/// local rather than re-exported so this test file is self-contained
/// and the production crate stays clean of test-only helpers.
fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join(name);
    {
        // Explicit File::create + write_all + sync_all + drop so the
        // writer fd is fully released before the child execs the
        // script — Linux returns ETXTBSY otherwise. Same workaround
        // hooks_executor_test.rs uses.
        let mut f = std::fs::File::create(&path).expect("create script");
        f.write_all(body.as_bytes()).expect("write script");
        f.sync_all().expect("sync script");
    }
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

/// **The H9 regression case.** A daemon child that writes ~1 MiB of
/// stderr per fire would, before the fix, deadlock the executor on
/// the second or third fire (whichever first hit the OS pipe buffer
/// limit). After the fix, the per-child stderr-drain task keeps the
/// pipe drained and all fires complete inside the timeout.
///
/// The script writes 1024 lines of `~1 KiB` of stderr per fire (~1
/// MiB total per fire, comfortably past the 64 KiB Linux / 16 KiB
/// macOS pipe buffer), then emits the NDJSON decision on stdout. We
/// drive 5 fires in sequence — under the broken executor the second
/// fire never returns. A 30s ceiling is generous slack; a real
/// regression hangs forever (capped by the test harness).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_mode_high_stderr_volume_no_deadlock() {
    let dir = tempfile::tempdir().expect("tempdir");
    // 1 MiB of stderr per fire. `yes` would be simpler but isn't
    // portable across BSD/GNU userland — a hand-rolled while loop
    // keeps the test working on macOS dev boxes and ubuntu CI alike.
    //
    // We use printf in a loop, writing ~1 KiB per iteration for
    // 1024 iterations. That's ~1 MiB of stderr per fire — well past
    // every platform's pipe buffer.
    let script = write_script(
        &dir,
        "noisy_daemon.sh",
        r#"#!/bin/sh
# 1 KiB chunk we'll write 1024 times per fire (1 MiB total).
chunk=$(printf 'x%.0s' $(seq 1 1023))
while IFS= read -r _line; do
  i=0
  while [ "$i" -lt 1024 ]; do
    printf '%s\n' "$chunk" >&2
    i=$((i + 1))
  done
  printf '%s\n' '{"action":"allow"}'
done
"#,
    );

    // 30s per-fire timeout — generous. A regressed executor hangs
    // forever; a working one returns in milliseconds even with the
    // 1 MiB stderr volume.
    let executor = DaemonExecutor::new(cfg_for(script, HookMode::Daemon, 30_000));

    let started = Instant::now();
    for i in 0..5u32 {
        let r = executor
            .fire(HookEvent::PostStore, json!({"i": i}))
            .await
            .unwrap_or_else(|e| panic!("fire {i} failed: {e}"));
        assert_eq!(
            r,
            HookDecision::Allow,
            "fire {i} returned {r:?}; expected Allow",
        );
    }
    let elapsed = started.elapsed();
    // 60s slack — even a slow CI runner should clear 5 MiB of
    // stderr piping in well under a minute. Anything close to this
    // bound suggests the drain task is missing or under-buffering.
    assert!(
        elapsed < Duration::from_secs(60),
        "5 fires of 1 MiB stderr each took {elapsed:?}; suggests drain task is missing",
    );

    let m = executor.metrics();
    assert_eq!(m.events_fired, 5);
    assert_eq!(
        m.events_dropped, 0,
        "no fire should have dropped under the H9 fix"
    );
}

/// Companion to the above: the *executor* timeout must still trip
/// cleanly when the child genuinely stops responding. A regressed
/// drain task that buffered unboundedly could mask a hung child by
/// keeping the pipe forever drainable; we want the executor to
/// surface `Timeout` in bounded wall-clock regardless.
///
/// The script writes one Allow then sleeps forever — the second fire
/// must trip the configured 500ms timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_mode_timeout_still_trips_with_drain_task_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "hang_after_one.sh",
        r#"#!/bin/sh
# Answer the first fire so we know the daemon connection is healthy,
# then go silent so the second fire trips the executor's timeout.
read -r _first
printf '%s\n' '{"action":"allow"}'
# Print a stderr breadcrumb so the drain task has something to buffer
# and the WARN-on-timeout log path actually has content to surface.
printf 'about to hang\n' >&2
sleep 60
"#,
    );

    let executor = DaemonExecutor::new(cfg_for(script, HookMode::Daemon, 500));

    // First fire warms the connection — must succeed.
    let r1 = executor
        .fire(HookEvent::PostStore, json!({"first": true}))
        .await
        .expect("first fire warms the daemon connection");
    assert_eq!(r1, HookDecision::Allow);

    // Second fire must trip Timeout (script is sleeping). The window
    // is generous — the configured budget is 500ms; if we don't see
    // an answer inside 5s the drain task itself is hung.
    let started = Instant::now();
    let r2 = executor
        .fire(HookEvent::PostStore, json!({"second": true}))
        .await;
    let elapsed = started.elapsed();
    assert!(
        matches!(r2, Err(ExecutorError::Timeout { .. })),
        "second fire should have surfaced Timeout, got {r2:?}",
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "Timeout took {elapsed:?}; bounded budget should be ~500ms",
    );
}

/// ExecExecutor counterpart — a one-shot child that writes stderr on
/// the *success* path. Before the H9 fix this stderr was silently
/// dropped (only the failure arm of `wait_with_output` kept it). The
/// fix logs it at DEBUG; this test asserts the executor still
/// returns the parsed decision cleanly even when stderr is non-empty,
/// so we don't regress the success path while plumbing the trace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exec_mode_stderr_on_success_path_does_not_break_decision() {
    let dir = tempfile::tempdir().expect("tempdir");
    let script = write_script(
        &dir,
        "noisy_allow.sh",
        r#"#!/bin/sh
# Drain stdin so the parent's stdin.shutdown() returns cleanly.
cat >/dev/null
# Write some stderr — operators reading the trace logs would expect
# to see this surfaced rather than silently dropped.
printf 'hook diagnostic: ran cleanup pass\n' >&2
printf 'hook diagnostic: 0 entries pruned\n' >&2
printf '%s\n' '{"action":"allow"}'
"#,
    );

    // 60s budget (was 30s, originally 5s) — issue #824: macOS-latest CI
    // runners have grown slower since 0536e96 bumped 5→30s; runs in
    // 2026-05-17 timed out at the 30s mark. Local runs finish in ~130ms.
    // Budget is for CI-flake resilience, not real workload. Real-deployment
    // hook timeouts are operator-configured. If this bump is also
    // insufficient, switch to #[cfg_attr(target_os = "macos", ignore)]
    // and file a runner-investigation follow-up.
    let executor = ExecExecutor::new(cfg_for(script, HookMode::Exec, 60_000));
    let r = executor
        .fire(HookEvent::PostStore, json!({}))
        .await
        .expect("fire");
    assert_eq!(r, HookDecision::Allow);

    let m = executor.metrics();
    assert_eq!(m.events_fired, 1);
    assert_eq!(m.events_dropped, 0);
}
