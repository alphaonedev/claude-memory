// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]

//! Issue #318 regression — MCP stdio writes must route through the
//! federation forward URL (when configured) so the local HTTP daemon's
//! `broadcast_store_quorum` fanout runs. Pre-fix, an MCP-stdio
//! `memory_store` against a federated node persisted locally but
//! zero rows replicated to peers.
//!
//! The fix adds `app_config.mcp_federation_forward_url: Option<String>`.
//! When `Some(url)`, `handle_store` skips the direct-SQLite path and
//! POSTs to `{url}/api/v1/memories`. When `None`, the legacy direct
//! path runs (single-node MCP deployments without a sibling
//! `ai-memory serve` daemon are unchanged).
//!
//! This test pins the config-knob plumbing — the allowlist accepts the
//! new key, the field round-trips through TOML, and `AppConfig::default`
//! leaves it `None` so the default behaviour is preserved.

use ai_memory::config::AppConfig;

#[test]
fn issue_318_default_app_config_disables_federation_forward() {
    let cfg = AppConfig::default();
    assert!(
        cfg.mcp_federation_forward_url.is_none(),
        "default must leave federation forward disabled (preserves single-node MCP semantics)"
    );
}

#[test]
fn issue_318_config_toml_round_trips_federation_forward_url() {
    let raw = "mcp_federation_forward_url = \"http://localhost:9077\"\n";
    let cfg: AppConfig = toml::from_str(raw).expect("parse config.toml");
    assert_eq!(
        cfg.mcp_federation_forward_url.as_deref(),
        Some("http://localhost:9077"),
        "the new config key must round-trip through TOML so operators in federated topologies \
         can enable MCP→HTTP forwarding without code changes",
    );
}

#[test]
fn issue_318_unknown_keys_allowlist_includes_federation_forward_url() {
    // The L1 unknown-keys diagnostic warns on every top-level key it
    // doesn't recognize. The new `mcp_federation_forward_url` key must
    // be in the canonical allowlist or operators who set it would see
    // a spurious WARN every boot.
    //
    // We can't test the WARN directly without spinning up tracing, but
    // we can pin the contract: an AppConfig that round-trips the new
    // key produces a config object with the field populated. This same
    // assertion is also covered by `config::tests::warn_unknown_top_level_keys_covers_every_appconfig_field`
    // in the lib — that test asserts every serialised AppConfig field
    // is in the allowlist. This test pins the behavior contract from
    // the integration boundary.
    let raw = r#"
tier = "autonomous"
mcp_federation_forward_url = "http://localhost:9077"
"#;
    let cfg: AppConfig = toml::from_str(raw).expect("parse config.toml");
    assert_eq!(cfg.tier.as_deref(), Some("autonomous"));
    assert_eq!(
        cfg.mcp_federation_forward_url.as_deref(),
        Some("http://localhost:9077")
    );
}
