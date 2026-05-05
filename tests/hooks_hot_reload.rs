// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — G7 hot-reload integration test.
//
// G1 (#554) shipped the `hooks.toml` schema plus a SIGHUP-driven
// reload task (`crate::hooks::config::spawn_reload_task`) that
// listens for SIGHUP and atomically swaps an `Arc<RwLock<Vec<HookConfig>>>`
// snapshot. G3 wired the executor that actually fires hooks. The
// G1 plumbing is observable end-to-end: spawn the reload task,
// flip the on-disk `hooks.toml`, send SIGHUP, observe that the
// snapshot now serves the new config to fresh fires while any
// already-running fire (built off the prior snapshot) still
// completes against the OLD config.
//
// This test is the deterministic e2e the G1 doc deferred to G3+
// once the executor existed. Linux + macOS only — Windows has no
// SIGHUP, and the daemon is a Unix-only deployment in practice.

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ai_memory::hooks::config::{HookConfigSnapshot, spawn_reload_task};
use ai_memory::hooks::{ChainResult, ExecutorRegistry, HookChain, HookConfig, HookEvent};
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::RwLock;

/// Write `body` to `dir/name`, mark it executable, return the
/// path. Mirrors `tests/hooks_executor_test.rs::write_script` —
/// see the comment there on the explicit fsync + drop dance that
/// avoids ETXTBSY on fast Linux runners.
fn write_script(dir: &TempDir, name: &str, body: &str) -> PathBuf {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let path = dir.path().join(name);
    {
        let mut f = std::fs::File::create(&path).expect("create script");
        f.write_all(body.as_bytes()).expect("write script");
        f.sync_all().expect("sync script");
    }
    let mut perms = std::fs::metadata(&path).expect("stat").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");
    path
}

/// Build a `hooks.toml` body that subscribes one `post_store`
/// hook to `command`. Marker text gets baked into the script so
/// callers can recover "which command did this fire actually
/// invoke?" by reading the side-effect file.
fn hooks_toml_for(command: &std::path::Path) -> String {
    format!(
        r#"
[[hook]]
event = "post_store"
command = {command:?}
priority = 0
timeout_ms = 5000
mode = "exec"
enabled = true
namespace = "*"
"#,
        command = command.display().to_string()
    )
}

/// Atomically replace `path` with `new_contents` so a concurrent
/// reader on the reload task never sees a half-written file. The
/// SIGHUP handler reopens the file on every signal so an atomic
/// rename guarantees the next read lands on the complete new
/// payload.
fn atomic_write(path: &std::path::Path, new_contents: &str) {
    let tmp = path.with_extension("toml.swap");
    std::fs::write(&tmp, new_contents).expect("stage swap file");
    std::fs::rename(&tmp, path).expect("atomic rename");
}

/// Snapshot the current contents of `marker_path` (an empty
/// string if the file does not exist). Each hook script appends a
/// known token to this file when it fires; the test recovers
/// "which version of the hook ran?" by diffing the marker.
fn read_marker(marker_path: &std::path::Path) -> String {
    std::fs::read_to_string(marker_path).unwrap_or_default()
}

/// Block until `predicate(snapshot)` returns true, polling every
/// 10ms up to `deadline`. Returns whether the predicate ever held.
/// Used to wait for the SIGHUP-driven reload to complete.
async fn poll_until<F>(
    snapshot: &Arc<HookConfigSnapshot>,
    mut predicate: F,
    deadline: Duration,
) -> bool
where
    F: FnMut(&[HookConfig]) -> bool,
{
    let start = Instant::now();
    while start.elapsed() < deadline {
        {
            let guard = snapshot.read().await;
            if predicate(&guard) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    false
}

/// Send SIGHUP to our own process. The reload task installed by
/// `spawn_reload_task` sits inside this process, so a self-kill
/// is the most deterministic delivery path (no pid race, no need
/// for a child process). `nix` would offer a nicer wrapper but
/// `libc` is already an unconditional dev-dep on the unix target
/// and `nix` is not.
fn self_sighup() {
    // Safety: `kill(2)` with SIGHUP on our own pid is signal-safe.
    // tokio's signal driver catches the signal off the async
    // runtime, so the test thread itself does not service it.
    let rc = unsafe {
        let pid = libc::getpid();
        libc::kill(pid, libc::SIGHUP)
    };
    assert_eq!(
        rc,
        0,
        "libc::kill SIGHUP failed: errno={}",
        std::io::Error::last_os_error()
    );
}

// ---------------------------------------------------------------------------
// The integration test.
// ---------------------------------------------------------------------------

/// Bundle of files + scripts the e2e flow operates over.
struct Fixture {
    _dir: TempDir,
    hooks_path: PathBuf,
    script_a: PathBuf,
    script_b: PathBuf,
    script_a_slow: PathBuf,
    marker_a: PathBuf,
    marker_b: PathBuf,
    marker_inflight: PathBuf,
}

/// Build the three hook scripts + an empty `hooks.toml` placeholder.
/// Each script appends a known token to its marker file when it
/// fires; the test recovers "which script ran?" by diffing markers.
fn build_fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let marker_a = dir.path().join("marker_a.log");
    let marker_b = dir.path().join("marker_b.log");
    let marker_inflight = dir.path().join("marker_inflight.log");

    let script_a = write_script(
        &dir,
        "hook_a.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
printf 'A' >> {marker:?}
printf '%s\n' '{{"action":"allow"}}'
"#,
            marker = marker_a.display().to_string()
        ),
    );
    let script_b = write_script(
        &dir,
        "hook_b.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
printf 'B' >> {marker:?}
printf '%s\n' '{{"action":"allow"}}'
"#,
            marker = marker_b.display().to_string()
        ),
    );
    // Same identity as A but sleeps 200ms before returning. Used
    // to model "in-flight at SIGHUP time" — the executor for the
    // slow A is constructed BEFORE the reload, then the reload
    // happens while the script is still running, then the slow
    // fire is awaited. It must complete on the OLD config (i.e.
    // write to marker_inflight, which is baked into this script
    // not script_b's marker).
    let script_a_slow = write_script(
        &dir,
        "hook_a_slow.sh",
        &format!(
            r#"#!/bin/sh
cat >/dev/null
sleep 0.2
printf 'A_INFLIGHT' >> {marker:?}
printf '%s\n' '{{"action":"allow"}}'
"#,
            marker = marker_inflight.display().to_string()
        ),
    );

    let hooks_path = dir.path().join("hooks.toml");
    Fixture {
        _dir: dir,
        hooks_path,
        script_a,
        script_b,
        script_a_slow,
        marker_a,
        marker_b,
        marker_inflight,
    }
}

/// Spawn an in-flight `post_store` chain fire off the *current*
/// snapshot. The returned `JoinHandle` only completes once the
/// child script exits — long enough that a SIGHUP lands while it
/// is still running.
fn spawn_inflight_fire(snapshot: Arc<HookConfigSnapshot>) -> tokio::task::JoinHandle<ChainResult> {
    tokio::spawn(async move {
        let hooks = snapshot.read().await.clone();
        let chain = HookChain::for_event(&hooks, HookEvent::PostStore);
        let mut registry = ExecutorRegistry::from_hooks(&hooks);
        chain
            .fire(
                HookEvent::PostStore,
                json!({"id": "in-flight"}),
                &mut registry,
            )
            .await
    })
}

/// End-to-end SIGHUP hot reload:
///
///   1. Write `hooks.toml` config A (`post_store` → `script_a`).
///   2. Build a [`HookConfigSnapshot`] from A and spawn the SIGHUP
///      reload task (the same plumbing G1 ships for production).
///   3. Fire `post_store` via a chain built off the *current*
///      snapshot. Assert `script_a`'s marker token landed.
///   4. Atomically replace `hooks.toml` with config B
///      (`post_store` → `script_b`).
///   5. Send SIGHUP to our own pid.
///   6. Poll the snapshot up to 500ms until it reports the new
///      config (one entry pointing at `script_b`).
///   7. Fire `post_store` again. Assert `script_b`'s marker token
///      landed and `script_a`'s count did NOT advance.
///   8. Verify the in-flight fire from before SIGHUP completes
///      against the OLD config — the executor was built off the
///      pre-reload snapshot and must not be retroactively
///      mutated by the swap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sighup_swaps_hooks_toml_without_restart() {
    let fx = build_fixture();
    atomic_write(&fx.hooks_path, &hooks_toml_for(&fx.script_a));

    let snapshot: Arc<HookConfigSnapshot> = Arc::new(RwLock::new(
        HookConfig::load_from_file(&fx.hooks_path).expect("load A"),
    ));

    // The reload task is the production plumbing under test —
    // spawn_reload_task installs the SIGHUP listener and swaps
    // the snapshot in place. We hold the JoinHandle so the test
    // can abort it on the way out for a clean shutdown.
    let reload_handle = spawn_reload_task(fx.hooks_path.clone(), Arc::clone(&snapshot));

    // Phase 1: A fires under config A.
    fire_post_store(&snapshot).await;
    assert_eq!(
        read_marker(&fx.marker_a),
        "A",
        "config A did not invoke script_a on first fire"
    );
    assert_eq!(
        read_marker(&fx.marker_b),
        "",
        "script_b somehow ran before config B was loaded"
    );

    // Kick off an in-flight fire on config A_slow. Swap the
    // snapshot to A_slow BEFORE the reload, then start the fire.
    // It will be sleeping when SIGHUP lands. The executor was
    // built from the pre-reload snapshot and holds its own
    // `HookConfig` clone — the swap must not interrupt it.
    {
        let mut guard = snapshot.write().await;
        *guard =
            HookConfig::load_from_str(&hooks_toml_for(&fx.script_a_slow)).expect("load A_slow");
    }
    let inflight = spawn_inflight_fire(Arc::clone(&snapshot));
    // Small yield so the in-flight task definitely starts its
    // child before we mutate the snapshot under it.
    tokio::time::sleep(Duration::from_millis(40)).await;

    // Replace hooks.toml with B and SIGHUP.
    atomic_write(&fx.hooks_path, &hooks_toml_for(&fx.script_b));
    self_sighup();

    let script_b_path = fx.script_b.clone();
    let reloaded = poll_until(
        &snapshot,
        |hooks| hooks.len() == 1 && hooks[0].command == script_b_path,
        Duration::from_millis(500),
    )
    .await;
    assert!(
        reloaded,
        "snapshot did not reflect config B within 500ms of SIGHUP"
    );

    // Phase 2: B fires under config B.
    fire_post_store(&snapshot).await;
    assert_eq!(
        read_marker(&fx.marker_b),
        "B",
        "config B did not invoke script_b after SIGHUP reload"
    );
    assert_eq!(
        read_marker(&fx.marker_a),
        "A",
        "script_a fired again after SIGHUP swap to config B"
    );

    // In-flight completes on its OLD config.
    let inflight_result = tokio::time::timeout(Duration::from_secs(5), inflight)
        .await
        .expect("in-flight fire timed out")
        .expect("in-flight join panicked");
    assert!(
        matches!(inflight_result, ChainResult::Allow),
        "in-flight fire returned non-Allow: {inflight_result:?}"
    );
    assert_eq!(
        read_marker(&fx.marker_inflight),
        "A_INFLIGHT",
        "in-flight A_slow fire did not complete on its captured pre-reload config"
    );

    reload_handle.abort();
}

/// Read the snapshot, build a `HookChain` for `PostStore`, fire
/// it through a fresh `ExecutorRegistry`, and assert the chain
/// returned `Allow`. Mirrors what `daemon_runtime`'s
/// `dispatch_event_with_hooks` would do at a real `memory_store`
/// call site.
async fn fire_post_store(snapshot: &Arc<HookConfigSnapshot>) {
    let hooks = snapshot.read().await.clone();
    let chain = HookChain::for_event(&hooks, HookEvent::PostStore);
    let mut registry = ExecutorRegistry::from_hooks(&hooks);
    let result = chain
        .fire(
            HookEvent::PostStore,
            json!({"id": "test", "namespace": "default"}),
            &mut registry,
        )
        .await;
    assert!(
        matches!(result, ChainResult::Allow),
        "post_store chain returned non-Allow: {result:?}"
    );
    // The marker-file side effect (asserted by the caller) is the
    // load-bearing proof the script actually ran; the chain
    // result above is the contract surface.
}
