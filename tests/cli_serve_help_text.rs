// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #703 — pin the `ai-memory serve --help` output so the
//! tier-resolution clarification can't silently regress.
//!
//! The brief for #700 originally listed `ai-memory serve --tier
//! autonomous` as a valid invocation. It isn't: the `serve`
//! subcommand has no `--tier` flag, and the daemon resolves its
//! effective feature tier from `config.toml` at startup. Per-
//! invocation tier overrides are only available on `mcp` /
//! `store` / `recall`. The help text must say so loudly enough
//! that an operator reading `--help` cannot miss it.

use assert_cmd::Command;

/// `ai-memory serve --help` must mention:
/// 1. that `--tier` is NOT a serve flag,
/// 2. where the daemon resolves its tier from (`config.toml`),
/// 3. which subcommands DO accept `--tier` for per-invocation overrides.
///
/// Each assertion below is intentionally fragmented so a regression on
/// one phrase fails loudly with a clear diff, rather than a single
/// long-string match that obscures which phrase went missing.
#[test]
fn serve_help_documents_tier_resolution() {
    let assert = Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args(["serve", "--help"])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    // (1) Says explicitly that --tier is not accepted on serve.
    assert!(
        stdout.contains("does NOT accept a `--tier` flag"),
        "serve --help must state that --tier is not accepted; got:\n{stdout}"
    );

    // (2) Points operator at config.toml as the resolution source.
    assert!(
        stdout.contains("config.toml"),
        "serve --help must reference config.toml as the tier source; got:\n{stdout}"
    );

    // (3) Names the per-invocation subcommands that DO take --tier.
    assert!(
        stdout.contains("mcp") && stdout.contains("store") && stdout.contains("recall"),
        "serve --help must point at mcp/store/recall for per-invocation tier; got:\n{stdout}"
    );
}

/// Smoke: `ai-memory serve --tier autonomous` must fail at parse time
/// with a clap-style "unexpected argument" error, not silently accept
/// the flag and ignore it. Pins the negative half of #703.
#[test]
fn serve_rejects_tier_flag() {
    let assert = Command::cargo_bin("ai-memory")
        .unwrap()
        .env("AI_MEMORY_NO_CONFIG", "1")
        .args(["serve", "--tier", "autonomous"])
        .assert()
        .failure();

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    // clap surfaces "unexpected argument" (or "found argument") when an
    // unrecognised long flag is passed. We accept either phrasing to
    // remain stable across clap minor versions.
    assert!(
        stderr.contains("unexpected argument")
            || stderr.contains("found argument")
            || stderr.contains("--tier"),
        "serve --tier must be rejected at parse time; stderr was:\n{stderr}"
    );
}
