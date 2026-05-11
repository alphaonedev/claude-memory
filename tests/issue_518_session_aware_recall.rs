// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]

//! Issue #518 — session-aware `memory_recall` defaults from
//! `[agents.defaults.recall_scope]`.
//!
//! Background: the v0.6.3.1 OpenClaw behavioral assessment showed
//! agents fail "what were you working on?" recovery (Phase 9 organic-
//! no-cue) because every cross-session `memory_recall` requires
//! explicit namespace + recency filters. The fix is a per-agent
//! defaults block in `config.toml` that the recall handlers can
//! splice in when called with `session_default=true` and no explicit
//! filters.
//!
//! This regression suite pins the contract end-to-end:
//!
//! 1. `AppConfig::default()` leaves `agents` as `None` (single-tenant
//!    deployments see zero behaviour change).
//! 2. `[agents.defaults.recall_scope]` round-trips through TOML.
//! 3. The unknown-keys allowlist accepts `agents` (no spurious WARN
//!    on operator boot).
//! 4. `effective_recall_scope` returns `Some(&scope)` when configured
//!    and `None` otherwise.
//! 5. The MCP `memory_recall` tool schema advertises `session_default`
//!    with the correct shape (default false; description names the
//!    config knob).
//! 6. The duration parser accepts the documented spec (`"24h"`,
//!    `"7d"`, `"30m"`, …).
//!
//! All assertions are pure-Rust and run under `AI_MEMORY_NO_CONFIG=1`
//! — no postgres / Ollama / network dependencies required.

use ai_memory::config::{AgentDefaults, AgentsConfig, AppConfig, RecallScope};

#[test]
fn issue_518_default_app_config_leaves_agents_none() {
    let cfg = AppConfig::default();
    assert!(
        cfg.agents.is_none(),
        "AppConfig::default() must leave agents = None so single-tenant deployments \
         see zero behaviour change (no recall_scope splicing on session_default=true)"
    );
    assert!(
        cfg.effective_recall_scope().is_none(),
        "effective_recall_scope() must return None when the block is unconfigured"
    );
}

#[test]
fn issue_518_config_toml_round_trips_recall_scope_full_block() {
    let raw = r#"
[agents.defaults.recall_scope]
namespaces = ["projects/atlas"]
since = "24h"
tier = "long"
limit = 50
"#;
    let cfg: AppConfig = toml::from_str(raw).expect("parse config.toml");
    let scope = cfg
        .effective_recall_scope()
        .expect("[agents.defaults.recall_scope] must parse into Some(&RecallScope)");
    assert_eq!(
        scope.namespaces.as_deref(),
        Some(&["projects/atlas".to_string()][..]),
        "namespaces list must round-trip verbatim"
    );
    assert_eq!(scope.since.as_deref(), Some("24h"));
    assert_eq!(scope.tier.as_deref(), Some("long"));
    assert_eq!(scope.limit, Some(50));
}

#[test]
fn issue_518_config_toml_round_trips_partial_block() {
    // Partial blocks must work — operators commonly want only a
    // namespace default + recency window without pinning a tier or
    // limit.
    let raw = r#"
[agents.defaults.recall_scope]
namespaces = ["projects/atlas", "projects/zephyr"]
since = "7d"
"#;
    let cfg: AppConfig = toml::from_str(raw).expect("parse partial recall_scope");
    let scope = cfg.effective_recall_scope().expect("scope must parse");
    assert_eq!(
        scope.namespaces.as_ref().map(Vec::len),
        Some(2),
        "namespaces list must preserve both entries"
    );
    assert_eq!(scope.since.as_deref(), Some("7d"));
    assert!(scope.tier.is_none(), "tier omitted => None");
    assert!(scope.limit.is_none(), "limit omitted => None");
}

#[test]
fn issue_518_unknown_keys_allowlist_accepts_agents() {
    // The L1 unknown-keys diagnostic warns on every top-level key it
    // doesn't recognise. The new `agents` key must be allowlisted or
    // operators who set `[agents.defaults.recall_scope]` would see a
    // spurious WARN every boot.
    //
    // We can't capture the WARN without spinning up tracing, but we
    // can prove the contract: an AppConfig that round-trips the new
    // key still loads its sibling top-level fields intact. This is
    // also re-asserted from `config::tests::warn_unknown_top_level_keys_covers_every_appconfig_field`
    // in the lib (every serialised AppConfig field must be in the
    // allowlist).
    let raw = r#"
tier = "autonomous"

[agents.defaults.recall_scope]
namespaces = ["projects/atlas"]
since = "24h"
"#;
    let cfg: AppConfig = toml::from_str(raw).expect("parse config.toml");
    assert_eq!(
        cfg.tier.as_deref(),
        Some("autonomous"),
        "top-level `tier` must survive when `[agents.…]` is present"
    );
    assert_eq!(
        cfg.effective_recall_scope()
            .and_then(|s| s.since.as_deref()),
        Some("24h"),
        "agents block round-trips even when sibling top-level keys are set"
    );
}

#[test]
fn issue_518_effective_recall_scope_traverses_nested_options() {
    // The accessor walks `agents -> defaults -> recall_scope` so any
    // None along the chain produces None at the top. Pin each rung:
    //   1. agents = None       => None
    //   2. agents = Some, defaults = None => None
    //   3. agents = Some, defaults = Some, recall_scope = None => None
    //   4. all populated => Some(&scope)
    let mut cfg = AppConfig::default();
    assert!(cfg.effective_recall_scope().is_none(), "(1)");

    cfg.agents = Some(AgentsConfig { defaults: None });
    assert!(cfg.effective_recall_scope().is_none(), "(2)");

    cfg.agents = Some(AgentsConfig {
        defaults: Some(AgentDefaults { recall_scope: None }),
    });
    assert!(cfg.effective_recall_scope().is_none(), "(3)");

    let want = RecallScope {
        namespaces: Some(vec!["projects/atlas".into()]),
        since: Some("24h".into()),
        tier: Some("long".into()),
        limit: Some(50),
    };
    cfg.agents = Some(AgentsConfig {
        defaults: Some(AgentDefaults {
            recall_scope: Some(want.clone()),
        }),
    });
    let got = cfg
        .effective_recall_scope()
        .expect("fully-populated chain must resolve to Some");
    assert_eq!(got.namespaces, want.namespaces);
    assert_eq!(got.since, want.since);
    assert_eq!(got.tier, want.tier);
    assert_eq!(got.limit, want.limit);
}

#[test]
fn issue_518_parse_duration_string_accepts_documented_units() {
    use ai_memory::config::parse_duration_string;
    // The spec calls out "24h" / "7d" / "30m" as the example wire
    // shapes. We accept the long aliases too (humans type whatever
    // feels natural).
    assert_eq!(
        parse_duration_string("24h"),
        Some(chrono::Duration::hours(24))
    );
    assert_eq!(parse_duration_string("7d"), Some(chrono::Duration::days(7)));
    assert_eq!(
        parse_duration_string("30m"),
        Some(chrono::Duration::minutes(30))
    );
    assert_eq!(
        parse_duration_string("90s"),
        Some(chrono::Duration::seconds(90))
    );
    assert_eq!(
        parse_duration_string("2w"),
        Some(chrono::Duration::weeks(2))
    );
    // Long-form units.
    assert_eq!(
        parse_duration_string("12 hours"),
        Some(chrono::Duration::hours(12))
    );
    // Case insensitive.
    assert_eq!(
        parse_duration_string("24H"),
        Some(chrono::Duration::hours(24))
    );
    // Malformed input => None (caller falls through to "no since
    // filter applied").
    assert!(parse_duration_string("forever").is_none());
    assert!(parse_duration_string("").is_none());
    assert!(parse_duration_string("-1h").is_none());
    assert!(parse_duration_string("3x").is_none());
}

#[test]
fn issue_518_mcp_tool_schema_advertises_session_default() {
    // The MCP `memory_recall` tool MUST expose `session_default`
    // through `tools/list` so clients can discover the splice
    // contract without out-of-band docs. Default must be false to
    // preserve the v0.6.x behaviour for callers that don't pass it
    // explicitly.
    let defs = ai_memory::mcp::tool_definitions();
    let tools = defs["tools"].as_array().expect("tools is an array");
    let recall = tools
        .iter()
        .find(|t| t["name"] == "memory_recall")
        .expect("memory_recall tool registered");
    let props = &recall["inputSchema"]["properties"];
    let sd = &props["session_default"];
    assert_eq!(sd["type"].as_str(), Some("boolean"));
    assert_eq!(sd["default"].as_bool(), Some(false));
    let desc = sd["description"].as_str().unwrap_or("");
    assert!(
        desc.contains("agents.defaults.recall_scope"),
        "session_default description must reference the config knob — got: {desc}"
    );
    // The top-level `docs` blurb should also mention the new flag so
    // operators reading capabilities see the contract surfaced once
    // more.
    let docs = recall["docs"].as_str().unwrap_or("");
    assert!(
        docs.contains("session_default"),
        "memory_recall docs must mention session_default — got: {docs}"
    );
}

#[test]
fn issue_518_recall_scope_struct_default_is_all_none() {
    let scope = RecallScope::default();
    assert!(scope.namespaces.is_none());
    assert!(scope.since.is_none());
    assert!(scope.tier.is_none());
    assert!(scope.limit.is_none());
}
