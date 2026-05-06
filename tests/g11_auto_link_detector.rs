// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 G11 (R3) — integration test for the reference
// `auto-link-detector` post_store hook in
// `tools/auto-link-detector/`.
//
// The reference binary lives in a sibling crate (not part of the
// `ai-memory` cargo package) so this test builds it on the fly via
// `cargo build --manifest-path tools/auto-link-detector/Cargo.toml`
// and then exercises the same stdio contract the production
// executor (`src/hooks/executor.rs::FireEnvelope`) writes.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};

/// Build the reference detector binary and return the absolute
/// path to it. Cached across tests in the same `cargo test`
/// invocation by relying on `cargo`'s own incremental build —
/// the second test pays only the manifest-load cost.
fn build_detector() -> PathBuf {
    let manifest_path = std::env::current_dir()
        .expect("cwd")
        .join("tools/auto-link-detector/Cargo.toml");
    assert!(
        manifest_path.exists(),
        "detector manifest missing at {}",
        manifest_path.display()
    );

    // Build into a per-test target dir so a parallel `cargo test`
    // invocation against the main crate doesn't race the
    // sibling-crate build cache.
    let target_dir = std::env::temp_dir().join("ai-memory-auto-link-detector-target");

    let status = Command::new("cargo")
        .args([
            "build",
            "--quiet",
            "--manifest-path",
            manifest_path.to_str().expect("utf-8 manifest path"),
            "--target-dir",
            target_dir.to_str().expect("utf-8 target dir"),
        ])
        .status()
        .expect("invoke cargo build");
    assert!(status.success(), "cargo build for detector failed");

    let bin = target_dir.join("debug").join("auto-link-detector");
    assert!(bin.exists(), "detector binary missing at {}", bin.display());
    bin
}

/// Pipe `envelope` to the detector in one-shot mode and return
/// the parsed decision JSON.
fn run_once(bin: &PathBuf, envelope: &Value) -> Value {
    use std::io::Write;
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn detector");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(envelope.to_string().as_bytes())
            .expect("write envelope");
    }
    let output = child.wait_with_output().expect("wait detector");
    assert!(
        output.status.success(),
        "detector exited non-zero: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let line = stdout
        .lines()
        .rfind(|l| !l.trim().is_empty())
        .expect("at least one decision line");
    serde_json::from_str(line).expect("decision parses")
}

// ---------------------------------------------------------------------------
// End-to-end: enabled hook + matching neighbour → auto-related link
// ---------------------------------------------------------------------------

/// The R3 acceptance test from the task brief: when the detector
/// is wired in and a memory is stored alongside a topically-similar
/// neighbour, an `auto-related` proposal appears on the resulting
/// decision with `attest_level = "R3"`.
#[test]
fn high_similarity_neighbour_emits_auto_related_link() {
    let bin = build_detector();

    let envelope = json!({
        "event": "post_store",
        "payload": {
            "id": "mem-new",
            "namespace": "team/eng",
            "title": "autovacuum knobs",
            "content": "Postgres autovacuum tuning: scale_factor and cost_delay knobs control bloat.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning notes for production bloat control.",
                },
                {
                    "id": "mem-unrelated",
                    "namespace": "team/eng",
                    "content": "Slack outage retro: Cloudflare DNS propagation lag pinned the issue.",
                }
            ],
        }
    });

    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "modify");

    let links = decision["delta"]["metadata"]["auto_related_links"]
        .as_array()
        .expect("auto_related_links array");
    assert!(!links.is_empty(), "expected at least one proposal");

    // The matching neighbour must be present; the unrelated one must not.
    let targets: Vec<&str> = links
        .iter()
        .map(|l| l.get("target").and_then(Value::as_str).unwrap_or(""))
        .collect();
    assert!(targets.contains(&"mem-neighbour"));
    assert!(!targets.contains(&"mem-unrelated"));

    for entry in links {
        assert_eq!(entry["source"], "mem-new");
        assert_eq!(entry["kind"], "auto-related");
        assert_eq!(entry["attest_level"], "R3");
        assert!(entry["score"].is_number());
    }
}

/// Wrong event class must fall through to `Allow` so the detector
/// is safe to attach to multiple chains.
#[test]
fn pre_store_event_falls_through_to_allow() {
    let bin = build_detector();
    let envelope = json!({
        "event": "pre_store",
        "payload": {
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning notes.",
                }
            ],
        }
    });
    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "allow");
}

/// No similar neighbour → no proposal → `Allow`.
#[test]
fn no_similar_neighbour_returns_allow() {
    let bin = build_detector();
    let envelope = json!({
        "event": "post_store",
        "payload": {
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Reverted PR #555 because it broke v3 capabilities matrix.",
            "recent_namespace_memories": [
                {
                    "id": "mem-neighbour",
                    "namespace": "team/eng",
                    "content": "Slack outage retro: Cloudflare DNS propagation lag pinned the issue.",
                }
            ],
        }
    });
    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "allow");
}

/// Cross-namespace candidates must never be linked, even when the
/// content is identical — the R3 brief restricts inference to
/// same-namespace neighbours.
#[test]
fn cross_namespace_neighbour_is_not_linked() {
    let bin = build_detector();
    let envelope = json!({
        "event": "post_store",
        "payload": {
            "id": "mem-new",
            "namespace": "team/eng",
            "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
            "recent_namespace_memories": [
                {
                    "id": "mem-other",
                    "namespace": "team/legal",
                    "content": "Postgres autovacuum scale_factor and cost_delay tuning.",
                }
            ],
        }
    });
    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "allow");
}
