// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Wave 2 Tier-A7 (issue #855) — pin the canonical environment-variable
//! precedence ladder + secret-classification invariant.
//!
//! CLAUDE.md §"Environment Variables" enumerates all 28 production
//! `AI_MEMORY_*` env vars and asserts the ladder:
//!
//! ```text
//! CLI flag  >  AI_MEMORY_* env var  >  config.toml field  >  compiled default
//! ```
//!
//! These tests mechanically enforce three properties that the table
//! alone cannot guarantee:
//!
//! 1. **`test_cli_flag_overrides_env`** — clap's `#[arg(long, env =
//!    "AI_MEMORY_DB")]` resolves an explicit CLI flag value above the
//!    env-var fallback. The test parses `--db /a.db` with
//!    `AI_MEMORY_DB=/b.db` in the process env and asserts the parsed
//!    field equals `/a.db`. A regression that flipped clap's precedence
//!    (e.g. by mis-using `default_value`) would surface here.
//!
//! 2. **`test_env_overrides_config`** — when no CLI flag is supplied,
//!    `AI_MEMORY_DB` overrides any `config.toml` `db = ...` setting.
//!    Verified through `AppConfig::effective_db`, the canonical
//!    resolution accessor every surface (CLI/daemon/MCP) calls. Both
//!    halves of the assertion run against the same `AppConfig` so the
//!    "config alone wins when env is unset" + "env wins when env is
//!    set" branches are pinned in one test.
//!
//! 3. **`test_secret_not_in_capabilities`** — `AI_MEMORY_DB_PASSPHRASE`
//!    is classified `secret` in the env-var table; the
//!    `memory_capabilities` JSON response must NEVER contain the
//!    plaintext passphrase. Hardens the no-secret-in-overlay invariant
//!    against a future refactor that absent-mindedly piped env state
//!    into the capabilities document (e.g. via a generic
//!    `env::vars()` walk).
//!
//! All three tests serialize env mutation through the shared
//! `EnvVarGuard` (process-wide `ENV_LOCK`) so parallel test execution
//! cannot race them.

use std::path::PathBuf;

use ai_memory::config::{AppConfig, FeatureTier, TierConfig};
use ai_memory::daemon_runtime::Cli;
use ai_memory::mcp::{CapabilitiesAccept, handle_capabilities_with_conn};
use clap::Parser;

mod common;
use common::EnvVarGuard;

/// Build the semantic-tier config the capabilities tests need without
/// pulling the embedder model into scope.
fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

// ---------------------------------------------------------------------------
// 1. CLI flag wins over env var (clap `#[arg(env = "AI_MEMORY_DB")]`).
// ---------------------------------------------------------------------------
#[test]
fn test_cli_flag_overrides_env() {
    // `AI_MEMORY_DB` is set to /b.db; the CLI explicitly passes
    // `--db /a.db`. clap's documented precedence is "CLI > env when the
    // CLI value is present", so the parsed `cli.db` MUST be /a.db.
    let _guard = EnvVarGuard::set("AI_MEMORY_DB", "/b.db".to_string());

    let cli = Cli::try_parse_from(["ai-memory", "--db", "/a.db", "stats"])
        .expect("clap parse must succeed");

    assert_eq!(
        cli.db,
        PathBuf::from("/a.db"),
        "CLI flag MUST override AI_MEMORY_DB env var; clap resolved {:?}",
        cli.db,
    );

    // Symmetric branch: when no CLI flag is passed, clap should fall
    // through to the env var. This pins the "env wins over compiled
    // default" half of the ladder and proves the previous assertion
    // wasn't passing because env-resolution is broken entirely.
    let cli_env_only =
        Cli::try_parse_from(["ai-memory", "stats"]).expect("clap parse must succeed (env-only)");
    assert_eq!(
        cli_env_only.db,
        PathBuf::from("/b.db"),
        "env var MUST be honored when --db is absent; clap resolved {:?}",
        cli_env_only.db,
    );
}

// ---------------------------------------------------------------------------
// 2. AI_MEMORY_DB env wins over config.toml when no CLI flag is set.
// ---------------------------------------------------------------------------
#[test]
fn test_env_overrides_config() {
    // Construct an AppConfig as if `config.toml` set `db = "/y.db"`.
    // `effective_db` honors a non-default CLI path; passing the default
    // `ai-memory.db` simulates "operator did not type --db on the
    // command line, only config.toml is in play".
    let cfg = AppConfig {
        db: Some("/y.db".to_string()),
        ..AppConfig::default()
    };
    let default_cli_db = PathBuf::from("ai-memory.db");

    // ---- Branch A: env unset → config wins over default ----
    let guard_a = EnvVarGuard::remove("AI_MEMORY_DB");
    let resolved_a = cfg.effective_db(&default_cli_db);
    assert_eq!(
        resolved_a,
        PathBuf::from("/y.db"),
        "config.toml db MUST win over compiled default when env is unset; got {resolved_a:?}",
    );
    drop(guard_a);

    // ---- Branch B: env "/x.db" overrides config "/y.db" ----
    // Because clap resolves env into the same field as --db, callers
    // see `cli.db = /x.db` and pass it to `effective_db`, which treats
    // any non-default value as explicit operator intent and bypasses
    // the config-file value.
    let _guard_b = EnvVarGuard::set("AI_MEMORY_DB", "/x.db".to_string());
    let cli =
        Cli::try_parse_from(["ai-memory", "stats"]).expect("clap parse must succeed (env-only)");
    let resolved_b = cfg.effective_db(&cli.db);
    assert_eq!(
        resolved_b,
        PathBuf::from("/x.db"),
        "AI_MEMORY_DB env MUST win over config.toml db; got {resolved_b:?}",
    );
}

// ---------------------------------------------------------------------------
// 3. AI_MEMORY_DB_PASSPHRASE secret value MUST NOT appear in the
//    memory_capabilities JSON response.
// ---------------------------------------------------------------------------
#[test]
fn test_secret_not_in_capabilities() {
    // Use a recognizable plaintext that a stray serializer would echo
    // verbatim. Distinct from any production constant.
    const SECRET: &str = "mysecret-config-precedence-canary-9f3a";

    let _guard = EnvVarGuard::set("AI_MEMORY_DB_PASSPHRASE", SECRET.to_string());

    // Sanity: the env var IS set so the test is exercising the path
    // we intend (a no-op secret-removal would otherwise trivially pass).
    assert_eq!(
        std::env::var("AI_MEMORY_DB_PASSPHRASE").as_deref(),
        Ok(SECRET),
        "EnvVarGuard must actually set the env var before we probe \
         memory_capabilities; without this sanity check the assertion \
         below would tautologically pass against an unset env."
    );

    let tier_config = semantic_tier();
    // `None` connection: the live-count overlays short-circuit, but the
    // capabilities document still serializes the runtime tier + features
    // + models surface, which is the exact surface a regression would
    // accidentally leak env state through.
    let response =
        handle_capabilities_with_conn(&tier_config, None, false, None, CapabilitiesAccept::V2)
            .expect("v2 capabilities serialize");

    let response_str =
        serde_json::to_string(&response).expect("capabilities response JSON-serializes");

    assert!(
        !response_str.contains(SECRET),
        "memory_capabilities response MUST NOT contain AI_MEMORY_DB_PASSPHRASE; \
         secret leaked into JSON: {response_str}",
    );

    // Also assert the env var NAME does not appear in the response —
    // a half-leak (key without value) would still be a defect because
    // it confirms to a reader that the daemon is reading the passphrase
    // from env (information disclosure).
    assert!(
        !response_str.contains("AI_MEMORY_DB_PASSPHRASE"),
        "memory_capabilities response MUST NOT mention the secret env-var \
         name either; got: {response_str}",
    );
}
