// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 I5 (R5) — integration test for the reference
// `transcript-extractor` pre_store hook in
// `tools/transcript-extractor/`.
//
// The reference binary lives in a sibling crate (not part of the
// `ai-memory` cargo package) so this test builds it on the fly via
// `cargo build --manifest-path tools/transcript-extractor/Cargo.toml`
// and then exercises the same stdio contract the production
// executor (`src/hooks/executor.rs::FireEnvelope`) writes.
//
// We also assert the namespace opt-in flag the I5 task added —
// `TranscriptsConfig::auto_extract_for` — so a regression that
// breaks the gate trips this test before the hook ever runs.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::{Command, Stdio};

use ai_memory::config::{TranscriptNamespaceConfig, TranscriptsConfig};
use serde_json::{Value, json};

/// Build the reference extractor binary and return the absolute
/// path to it. Cached across tests in the same `cargo test`
/// invocation by relying on `cargo`'s own incremental build —
/// the second test pays only the manifest-load cost.
fn build_extractor() -> PathBuf {
    let manifest_path = std::env::current_dir()
        .expect("cwd")
        .join("tools/transcript-extractor/Cargo.toml");
    assert!(
        manifest_path.exists(),
        "extractor manifest missing at {}",
        manifest_path.display()
    );

    // Build into a per-test target dir so a parallel `cargo test`
    // invocation against the main crate doesn't race the
    // sibling-crate build cache.
    let target_dir = std::env::temp_dir().join("ai-memory-transcript-extractor-target");

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
    assert!(status.success(), "cargo build for extractor failed");

    let bin = target_dir.join("debug").join("transcript-extractor");
    assert!(
        bin.exists(),
        "extractor binary missing at {}",
        bin.display()
    );
    bin
}

/// Pipe `envelope` to the extractor in one-shot mode and return
/// the parsed decision JSON.
fn run_once(bin: &PathBuf, envelope: &Value) -> Value {
    use std::io::Write;
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn extractor");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin
            .write_all(envelope.to_string().as_bytes())
            .expect("write envelope");
    }
    let output = child.wait_with_output().expect("wait extractor");
    assert!(
        output.status.success(),
        "extractor exited non-zero: stderr={}",
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
// End-to-end: enabled hook + transcript memory → extracted memories
// ---------------------------------------------------------------------------

/// The R5 acceptance test from the task brief: when the
/// extractor is wired in and a transcript is stored, the extracted
/// memories appear on the resulting decision.
#[test]
fn enabled_hook_extracts_memories_from_transcript() {
    let bin = build_extractor();

    let content = "User: how does v0.7 hooks chain ordering work?\n\
        Assistant: G5 sorts by priority, ties broken by file order, first deny wins.\n\n\
        User: what's the per-event-class timeout?\n\
        Assistant: G6 lets operators name a timeout per event family in hooks.toml.\n\n\
        User: where does the daemon executor live?\n\
        Assistant: src/hooks/executor.rs houses both ExecExecutor and DaemonExecutor.";

    let envelope = json!({
        "event": "pre_store",
        "payload": {
            "namespace": "transcript/agent",
            "title": "v0.7 hooks Q&A",
            "content": content,
            "metadata": { "kind": "transcript" },
        }
    });

    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "modify");

    let extracted = &decision["delta"]["metadata"]["extracted_memories"];
    assert!(extracted.is_array(), "extracted_memories must be an array");
    let arr = extracted.as_array().unwrap();
    assert!(
        !arr.is_empty(),
        "at least one paragraph should survive the heuristic"
    );
    for entry in arr {
        assert!(entry["title"].is_string());
        assert!(entry["content"].is_string());
        assert!(entry["score"].is_number());
        assert!(entry["span_start"].is_number());
        assert!(entry["span_end"].is_number());
    }
}

/// Non-transcript memory in a non-transcript namespace must NOT
/// trigger extraction, even when the hook is wired in. This
/// guards the substrate from misfiring on every `pre_store` fire.
#[test]
fn non_transcript_memory_returns_allow() {
    let bin = build_extractor();
    let envelope = json!({
        "event": "pre_store",
        "payload": {
            "namespace": "team/eng",
            "title": "rollback note",
            "content": "Reverted PR #555 because it broke v3 capabilities.",
        }
    });
    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "allow");
}

/// The wrong event class must fall through to `Allow` so the
/// extractor is safe to attach to multiple chains.
#[test]
fn post_store_event_falls_through_to_allow() {
    let bin = build_extractor();
    let envelope = json!({
        "event": "post_store",
        "payload": {
            "namespace": "transcript/agent",
            "content": "User: x\nAssistant: y\n\nUser: z\nAssistant: w",
        }
    });
    let decision = run_once(&bin, &envelope);
    assert_eq!(decision["action"], "allow");
}

// ---------------------------------------------------------------------------
// Opt-in resolver — exercises the config knob the hook chain consults
// before it ever fires the extractor.
// ---------------------------------------------------------------------------

#[test]
fn auto_extract_resolver_gates_namespace_correctly() {
    let mut nss = std::collections::HashMap::new();
    nss.insert(
        "transcript/agent".into(),
        TranscriptNamespaceConfig {
            auto_extract: Some(true),
            ..Default::default()
        },
    );
    nss.insert(
        "team/legal/*".into(),
        TranscriptNamespaceConfig {
            auto_extract: Some(false),
            ..Default::default()
        },
    );
    let cfg = TranscriptsConfig {
        namespaces: Some(nss),
        ..Default::default()
    };

    // Exact match wins.
    assert!(cfg.auto_extract_for("transcript/agent"));
    // Prefix opt-out fires under the `/*` pattern.
    assert!(!cfg.auto_extract_for("team/legal/contracts"));
    // Anything else: default off.
    assert!(!cfg.auto_extract_for("anything/else"));
}

#[test]
fn auto_extract_resolver_default_off_when_no_block() {
    let cfg = TranscriptsConfig::default();
    assert!(!cfg.auto_extract_for("transcript/agent"));
}
