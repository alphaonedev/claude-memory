// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the **Capabilities v3 schema** — A1 increment of
//! the v0.7.0 `attested-cortex` epic (track A, issue #545).
//!
//! v3 is additive over v2 and adds a top-level `summary` field carrying a
//! pre-computed plain-language description of the LLM's operational tool
//! surface (loaded count, total count, three named recovery paths for
//! reaching unloaded families). Computed at response time from the live
//! `Profile` state so the count of advertised tools always matches what
//! the running server actually advertises in `tools/list`.
//!
//! Future v0.7.0 increments (A2-A4) extend v3 with `to_describe_to_user`,
//! per-tool `callable_now`, and `agent_permitted_families`. A5 bumps the
//! default wire shape and seals v3 as the recommended client target.
//!
//! These tests pin the A1 contract:
//! - `accept="v3"` returns a document with `schema_version="3"` and a
//!   non-empty `summary` field.
//! - `summary` carries the live profile's loaded vs total tool counts and
//!   names the three recovery paths.
//! - The v1/v2 entry point refuses `V3` (with a clear error message) so a
//!   miswired caller fails loud rather than serving a stale shape.
//! - v2 callers see no behavior change (backward compat).

use ai_memory::config::{Capabilities, CapabilitiesV3, FeatureTier, TierConfig};
use ai_memory::mcp::{
    CapabilitiesAccept, build_capabilities_summary, handle_capabilities_with_conn,
    handle_capabilities_with_conn_v3,
};
use ai_memory::profile::Profile;
use serde_json::Value;

/// Build a fresh in-memory `rusqlite::Connection` so each test gets a
/// clean DB state for the live-count overlays.
fn fresh_conn() -> rusqlite::Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

// ---------------------------------------------------------------------------
// CapabilitiesAccept::parse("v3") and ("3") both resolve to V3.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_accept_parses_v3_alias() {
    assert_eq!(CapabilitiesAccept::parse("v3"), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse("3"), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse("V3"), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse(" v3 "), CapabilitiesAccept::V3);
}

// ---------------------------------------------------------------------------
// CapabilitiesAccept::parse on unknown values still falls back to V2 (A1
// preserves v0.6.3.1 behavior — only "v1"/"1" and "v3"/"3" route away from
// the v2 default).
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_unknown_accept_still_falls_back_to_v2() {
    assert_eq!(CapabilitiesAccept::parse("bogus"), CapabilitiesAccept::V2);
    assert_eq!(CapabilitiesAccept::parse(""), CapabilitiesAccept::V2);
    assert_eq!(CapabilitiesAccept::parse("v9"), CapabilitiesAccept::V2);
}

// ---------------------------------------------------------------------------
// The v1/v2 entry point refuses V3 with a clear error message — v3 needs
// the live `Profile` for summary computation, which the legacy signature
// can't carry. A miswired caller must fail loud rather than serve a stale
// v2 shape under the v3 label.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_legacy_entry_point_refuses_v3() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let err = handle_capabilities_with_conn(
        &tier_config,
        None,
        false,
        Some(&conn),
        CapabilitiesAccept::V3,
    )
    .expect_err("legacy entry point must refuse V3");
    assert!(
        err.contains("v3 requires profile context"),
        "error must direct caller to handle_capabilities_with_conn_v3, got: {err}"
    );
    assert!(err.contains("handle_capabilities_with_conn_v3"));
}

// ---------------------------------------------------------------------------
// v3 entry point returns a document with schema_version="3" and a
// non-empty summary field.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_response_carries_schema_version_and_summary() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
    )
    .expect("v3 capabilities serialize");

    assert_eq!(
        val["schema_version"], "3",
        "v3 must carry schema_version=\"3\"; got {val}"
    );
    let summary = val["summary"]
        .as_str()
        .expect("summary must be present and stringy");
    assert!(
        !summary.is_empty(),
        "summary must be non-empty under v3, got: {summary:?}"
    );
}

// ---------------------------------------------------------------------------
// summary on the `core` profile honestly reports 6 of 43 visible (5 core
// tools + memory_capabilities always-on bootstrap), labels the profile
// "core", and references all three named recovery paths.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_core_profile_counts_and_names_recovery_paths() {
    let summary = build_capabilities_summary(&Profile::core());

    // Visible = 5 core + 1 always-on (`memory_capabilities` lives in
    // Family::Meta which the core profile doesn't load, so the bootstrap
    // injection adds it back).
    assert!(
        summary.starts_with("6 of 43 tools"),
        "core profile summary should open with \"6 of 43 tools\"; got: {summary}"
    );
    assert!(summary.contains("(core)"), "must label the profile as core");
    assert!(
        summary.contains("37 are listed in this manifest"),
        "core profile must report 37 unloaded (43 - 6); got: {summary}"
    );

    // Three named recovery paths must all appear (verbatim names — these
    // are the strings reasoning-class LLMs are expected to repeat back).
    assert!(summary.contains("--profile <family>"));
    assert!(summary.contains("memory_load_family(family=<name>)"));
    assert!(summary.contains("memory_smart_load(intent="));
    assert!(summary.contains("JSON-RPC -32601"));
}

// ---------------------------------------------------------------------------
// summary on the `full` profile reports 43 of 43 visible, 0 unloaded, and
// labels the profile "full". The recovery paths are still listed —
// they're the canonical recovery vocabulary the LLM gets calibrated on
// regardless of the current profile state.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_full_profile_reports_all_visible() {
    let summary = build_capabilities_summary(&Profile::full());

    assert!(
        summary.starts_with("43 of 43 tools"),
        "full profile summary should open with \"43 of 43 tools\"; got: {summary}"
    );
    assert!(summary.contains("(full)"));
    assert!(
        summary.contains("0 are listed in this manifest"),
        "full profile must report 0 unloaded; got: {summary}"
    );
    // Even when nothing is unloaded, the recovery vocabulary stays present
    // so an LLM exposed only to the full-profile summary still learns the
    // names of the loader tools.
    assert!(summary.contains("memory_load_family"));
    assert!(summary.contains("memory_smart_load"));
}

// ---------------------------------------------------------------------------
// summary on the `graph` profile counts 13 visible (5 core + 8 graph) +
// the bootstrap, labels the profile "graph", and reports the rest as
// unloaded.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_graph_profile_counts() {
    let summary = build_capabilities_summary(&Profile::graph());
    assert!(
        summary.starts_with("14 of 43 tools"),
        "graph profile = 5 core + 8 graph + 1 always-on bootstrap = 14; got: {summary}"
    );
    assert!(summary.contains("(graph)"));
    assert!(summary.contains("29 are listed in this manifest"));
}

// ---------------------------------------------------------------------------
// CapabilitiesV3 round-trips through serde — schema_version, summary, and
// every v2 field must survive serialize → deserialize.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_struct_round_trips_through_serde() {
    let tier_config = semantic_tier();
    let caps: Capabilities = tier_config.capabilities();
    let v3 = caps.to_v3("hello operator".to_string());

    let json = serde_json::to_value(&v3).expect("serialize v3");
    let back: CapabilitiesV3 = serde_json::from_value(json.clone()).expect("deserialize v3");

    assert_eq!(back.schema_version, "3");
    assert_eq!(back.summary, "hello operator");
    assert_eq!(back.tier, v3.tier);
    assert_eq!(back.version, v3.version);
    // Sanity that the v2 sub-blocks are present.
    assert!(json.get("features").is_some());
    assert!(json.get("models").is_some());
    assert!(json.get("permissions").is_some());
    assert!(json.get("hooks").is_some());
    assert!(json.get("compaction").is_some());
    assert!(json.get("approval").is_some());
    assert!(json.get("transcripts").is_some());
}

// ---------------------------------------------------------------------------
// v3 response includes the same v2 sub-blocks (features.embedder_loaded,
// permissions, hooks, etc.) so a v3 client doesn't lose any v2 data.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_preserves_v2_sub_blocks() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val: Value = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        true, // embedder loaded
        Some(&conn),
        &Profile::full(),
    )
    .expect("v3 capabilities serialize");

    assert_eq!(val["features"]["embedder_loaded"], true);
    assert_eq!(val["features"]["recall_mode_active"], "hybrid");
    assert!(val["models"]["embedding"].is_string());
    assert_eq!(val["permissions"]["active_rules"], 0);
    assert_eq!(val["hooks"]["registered_count"], 0);
    assert_eq!(val["approval"]["pending_requests"], 0);
}

// ---------------------------------------------------------------------------
// v2 callers still get the v2 shape (no schema_version="3", no summary
// field) — A1 is additive; A5 will be the schema bump.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_v2_callers_unaffected_by_a1() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        false,
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities still work");

    assert_eq!(val["schema_version"], "2");
    assert!(
        val.get("summary").is_none(),
        "v2 must not gain the v3 summary field"
    );
}
