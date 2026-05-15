// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the **Capabilities v3 schema** — A1+A2 increments
//! of the v0.7.0 `attested-cortex` epic (track A, issue #545).
//!
//! v3 is additive over v2 and adds two top-level pre-computed strings:
//! - `summary` (A1) — terse description of operational access plus the
//!   three named recovery paths.
//! - `to_describe_to_user` (A2) — plain-English, end-user-facing
//!   sentence the LLM should repeat verbatim when an end-user asks
//!   "what tools do you have?". No MCP jargon.
//!
//! Both are computed at response time from the live `Profile` state so
//! the count of advertised tools always matches what the running server
//! actually advertises in `tools/list`.
//!
//! Future v0.7.0 increments (A3-A4) extend v3 with per-tool
//! `callable_now` and `agent_permitted_families`. A5 bumps the default
//! wire shape and seals v3 as the recommended client target.
//!
//! These tests pin the A1+A2 contract:
//! - `accept="v3"` returns a document with `schema_version="3"`, a
//!   non-empty `summary`, and a non-empty `to_describe_to_user`.
//! - `summary` carries the live profile's loaded vs total tool counts
//!   and names the three recovery paths.
//! - `to_describe_to_user` reads as plain English, omits MCP jargon,
//!   and accurately reports the loaded-tool count + names.
//! - The v1/v2 entry point refuses `V3` (with a clear error message)
//!   so a miswired caller fails loud rather than serving a stale shape.
//! - v2 callers see no behavior change (backward compat).

use ai_memory::config::{Capabilities, CapabilitiesV3, FeatureTier, McpConfig, TierConfig};
use ai_memory::harness::Harness;
use ai_memory::mcp::{
    CapabilitiesAccept, build_agent_permitted_families, build_capabilities_describe_to_user,
    build_capabilities_summary, build_capabilities_tools, handle_capabilities_with_conn,
    handle_capabilities_with_conn_v3,
};
use ai_memory::profile::Profile;
use serde_json::Value;
use std::collections::HashMap;

/// v0.7.0 A3 — build a minimal `[mcp.allowlist]` table for tests.
fn allowlist(rows: &[(&str, &[&str])]) -> McpConfig {
    let mut map = HashMap::new();
    for (agent, fams) in rows {
        map.insert(
            (*agent).to_string(),
            fams.iter().map(|s| (*s).to_string()).collect(),
        );
    }
    McpConfig {
        profile: None,
        allowlist: Some(map),
    }
}

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
// CapabilitiesAccept::parse on unknown / missing values falls back to V3
// since v0.7.0 A5 (was V2 in A1–A4). Explicit `"v1"`/`"v2"` still resolve
// to their respective shapes.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_unknown_accept_falls_back_to_v3_after_a5() {
    assert_eq!(CapabilitiesAccept::parse("bogus"), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse(""), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse("v9"), CapabilitiesAccept::V3);
    // Sanity: explicit pinning still works.
    assert_eq!(CapabilitiesAccept::parse("v2"), CapabilitiesAccept::V2);
    assert_eq!(CapabilitiesAccept::parse("v1"), CapabilitiesAccept::V1);
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
        None,
        None,
        None,
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
// summary on the `core` profile honestly reports 8 of 51 visible (7 core
// tools — including v0.7 B1 `memory_load_family` and v0.7 B2
// `memory_smart_load` — plus the memory_capabilities always-on
// bootstrap), labels the profile "core", and references all three named
// recovery paths. (Total bumped from 43 to 44 in v0.7.0 I4 —
// Family::Graph gained `memory_replay`; 44 to 45 in v0.7 H4 —
// Family::Graph gained `memory_verify`; 45 to 46 in v0.7 B1 —
// Family::Core gained `memory_load_family`; 46 to 48 in v0.7 K7
// — Family::Power gained `memory_subscription_replay` +
// `memory_subscription_dlq_list`; 48 to 49 in v0.7 J7 —
// Family::Graph gained `memory_find_paths`; 49 to 50 in v0.7 B2 —
// Family::Core gained `memory_smart_load`; 50 to 51 in v0.7 K8 —
// Family::Power gained `memory_quota_status`.)
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_core_profile_counts_and_names_recovery_paths() {
    let summary = build_capabilities_summary(&Profile::core());

    // Round-2 F13 — summary now reports the substantive "memory tool"
    // count (excluding the always-on `memory_capabilities` bootstrap)
    // so it agrees with `build_capabilities_describe_to_user`'s
    // "{n_loaded} memory tool{s}" phrasing. Core profile loads
    // `Family::Core` (7 tools) and does NOT load `Family::Meta`, so
    // visible memory tools = 7 (the bootstrap is plumbing, not a
    // memory tool). Total memory tools = 61 - 1 = 60 (60 substantive;
    // bumped via v0.7.0 L1-5 5×memory_skill_* + v0.7.0 L2-7
    // memory_skill_compositional_context).
    assert!(
        summary.starts_with("7 of 65 memory tools"),
        "core profile summary should open with \"7 of 65 memory tools\" (Round-2 F13; \
         v0.7.0 issue #691 added memory_check_agent_action + memory_rule_list, \
         v0.7.0 L1-5 added 5 memory_skill_* tools to Family::Other, v0.7.0 L2-3 \
         added memory_dependents_of_invalidated to Family::Power, v0.7.0 L2-6 \
         added memory_skill_promote_from_reflection to Family::Other, v0.7.0 \
         L2-7 added memory_skill_compositional_context to Family::Other, v0.7.0 \
         QW-1 added memory_export_reflection to Family::Power, and v0.7.0 QW-3 \
         follow-up added memory_offload + memory_deref to Family::Power — \
         bumping the substantive total from 52 to 65); got: {summary}"
    );
    assert!(summary.contains("(core)"), "must label the profile as core");
    assert!(
        summary.contains("58 are listed in this manifest"),
        "core profile must report 58 unloaded (65 - 7); got: {summary}"
    );

    // Three named recovery paths must all appear (verbatim names — these
    // are the strings reasoning-class LLMs are expected to repeat back).
    assert!(summary.contains("--profile <family>"));
    assert!(summary.contains("memory_load_family(family=<name>)"));
    assert!(summary.contains("memory_smart_load(intent="));
    assert!(summary.contains("JSON-RPC -32601"));
}

// ---------------------------------------------------------------------------
// summary on the `full` profile reports 51 of 51 visible, 0 unloaded, and
// labels the profile "full". The recovery paths are still listed —
// they're the canonical recovery vocabulary the LLM gets calibrated on
// regardless of the current profile state. (Total bumped from 43 to 44
// in v0.7.0 I4 — Family::Graph gained `memory_replay`; 44 to 45 in v0.7
// H4 — Family::Graph gained `memory_verify`; 45 to 46 in v0.7 B1 —
// Family::Core gained `memory_load_family`; 46 to 48 in v0.7 K7 —
// Family::Power gained `memory_subscription_replay` +
// `memory_subscription_dlq_list`; 48 to 49 in v0.7 J7 —
// Family::Graph gained `memory_find_paths`; 49 to 50 in v0.7 B2 —
// Family::Core gained `memory_smart_load`; 50 to 51 in v0.7 K8 —
// Family::Power gained `memory_quota_status`.)
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_full_profile_reports_all_visible() {
    let summary = build_capabilities_summary(&Profile::full());

    // Round-2 F13 — summary aligns with describe_to_user's "all 56
    // memory tools" phrasing. Full profile loads every family
    // (visible = 56 substantive memory tools; the
    // `memory_capabilities` bootstrap is excluded from the count to
    // match the user-facing string). v0.7.0 L1-5 added
    // 5 memory_skill_* tools to Family::Other, bumping the substantive
    // total from 51 to 56.
    assert!(
        summary.starts_with("65 of 65 memory tools"),
        "full profile summary should open with \"65 of 65 memory tools\" (Round-2 F13; \
         v0.7.0 issue #691 added memory_check_agent_action + memory_rule_list, \
         v0.7.0 L1-5 added 5 memory_skill_* tools, v0.7.0 L2-3 added \
         memory_dependents_of_invalidated, v0.7.0 L2-6 added \
         memory_skill_promote_from_reflection, v0.7.0 L2-7 added \
         memory_skill_compositional_context, v0.7.0 QW-1 added \
         memory_export_reflection, and v0.7.0 QW-3 follow-up added \
         memory_offload + memory_deref); got: {summary}"
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
// summary on the `graph` profile counts 18 visible (7 core after v0.7
// B1 + B2 + 11 graph after v0.7 J7) + the bootstrap, labels the
// profile "graph", and reports the rest as unloaded.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_summary_graph_profile_counts() {
    let summary = build_capabilities_summary(&Profile::graph());
    // Round-2 F13 — summary uses substantive "memory tool" count.
    // Graph profile = 7 core (v0.7 B1+B2) + 11 graph (v0.7 J7) = 18
    // memory tools. Total = 55 (56 - bootstrap; v0.7.0 L1-5 added 5
    // memory_skill_* tools to Family::Other, bumping total from 51 to 56).
    assert!(
        summary.starts_with("18 of 65 memory tools"),
        "graph profile = 7 core (v0.7 B1+B2) + 11 graph (v0.7 J7) = 18 memory tools \
         (Round-2 F13: bootstrap excluded; v0.7.0 issue #691 added two power tools, \
         v0.7.0 L1-5 added 5 memory_skill_* tools to Family::Other, v0.7.0 L2-3 \
         added memory_dependents_of_invalidated, v0.7.0 L2-6 added \
         memory_skill_promote_from_reflection, v0.7.0 L2-7 added \
         memory_skill_compositional_context, v0.7.0 QW-1 added \
         memory_export_reflection, and v0.7.0 QW-3 follow-up added \
         memory_offload + memory_deref to Family::Power, bumping the substantive \
         total from 52 to 65); got: {summary}"
    );
    assert!(summary.contains("(graph)"));
    assert!(summary.contains("47 are listed in this manifest"));
}

// ---------------------------------------------------------------------------
// CapabilitiesV3 round-trips through serde — schema_version, summary,
// to_describe_to_user (A2), and every v2 sub-block must survive
// serialize → deserialize.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_struct_round_trips_through_serde() {
    let tier_config = semantic_tier();
    let caps: Capabilities = tier_config.capabilities();
    let v3 = caps.to_v3(
        "hello operator".to_string(),
        "hello human".to_string(),
        Vec::new(),
        None,
        None,
    );

    let json = serde_json::to_value(&v3).expect("serialize v3");
    let back: CapabilitiesV3 = serde_json::from_value(json.clone()).expect("deserialize v3");

    assert_eq!(back.schema_version, "3");
    assert_eq!(back.summary, "hello operator");
    assert_eq!(back.to_describe_to_user, "hello human");
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
// A2: v3 response carries a non-empty top-level `to_describe_to_user`
// field, distinct from the A1 `summary` field.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_response_carries_to_describe_to_user() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("to_describe_to_user must be present and stringy under v3");
    assert!(
        !describe.is_empty(),
        "to_describe_to_user must be non-empty under v3, got: {describe:?}"
    );
    let summary = val["summary"].as_str().expect("summary present");
    assert_ne!(
        describe, summary,
        "to_describe_to_user must be a distinct sentence from summary"
    );
}

// ---------------------------------------------------------------------------
// A2: to_describe_to_user on `core` profile reads as plain English,
// names the loaded tools by short name (no `memory_` prefix), reports
// 40 unloaded with a sample, and ends with the canonical end-user
// recovery hint.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_describe_core_profile_is_plain_english_with_loaded_names() {
    let describe = build_capabilities_describe_to_user(&Profile::core());

    // Opens with the canonical "I can directly use N memory tool(s)"
    // form. v0.7 B1 + B2 — Core gained memory_load_family +
    // memory_smart_load so loaded count is now 7, and the preview
    // overflows the 5-name cap (ends in ", ...").
    assert!(
        describe.starts_with("I can directly use 7 memory tools right now ("),
        "core profile describe must open canonically; got: {describe}"
    );
    // Loaded preview lists the first 5 core tool names with the
    // memory_ prefix STRIPPED (no MCP jargon for end users), followed
    // by ", ..." since core now ships 7 tools (v0.7 B1 + B2).
    assert!(describe.contains("(store, recall, list, get, search, ...)"));
    // Reports the unloaded count. 49 = 55 user-relevant tools − 7
    // core. (55 = 56 total tools − 1 always-on bootstrap.) The
    // bootstrap (`memory_capabilities`) is excluded from both sides
    // for honest user-facing counting. Total bumped to 56 in v0.7.0
    // L1-5 (Family::Other gained 5 memory_skill_* tools).
    assert!(
        describe.contains("58 more"),
        "core profile must report 58 unloaded (65 - 7); v0.7.0 issue #691 \
         added memory_check_agent_action + memory_rule_list, v0.7.0 L1-5 added \
         5 memory_skill_* tools, v0.7.0 L2-3 added \
         memory_dependents_of_invalidated, v0.7.0 L2-6 added \
         memory_skill_promote_from_reflection, v0.7.0 L2-7 added \
         memory_skill_compositional_context, v0.7.0 QW-1 added \
         memory_export_reflection, and v0.7.0 QW-3 follow-up added \
         memory_offload + memory_deref, bumping the substantive total to 65; \
         got: {describe}"
    );
    // Sample of unloaded tools is plain (no memory_ prefix). The first
    // four unloaded under core are lifecycle's update/delete/forget/gc.
    assert!(describe.contains("update, delete, forget, gc"));
    // Ends with the end-user-facing recovery hint, not the operator
    // recovery vocabulary used by `summary`.
    assert!(describe.contains("available on demand"));
    assert!(describe.contains("restart the server with a different profile"));

    // Tone constraint (A2): NO MCP jargon. The describe sentence must
    // not surface CLI flags, JSON-RPC error codes, or runtime tool
    // names a user wouldn't recognize.
    assert!(!describe.contains("--profile <family>"));
    assert!(!describe.contains("memory_load_family"));
    assert!(!describe.contains("memory_smart_load"));
    assert!(!describe.contains("JSON-RPC"));
    assert!(!describe.contains("-32601"));
    assert!(!describe.contains("memory_"));
    assert!(!describe.contains("tools/list"));
}

// ---------------------------------------------------------------------------
// A2: to_describe_to_user on `full` profile reports all 51 tools loaded
// (ALWAYS_ON_TOOLS bootstrap is excluded from the user-facing count) and
// uses the "nothing more to load" closing form rather than the recovery
// hint. (Bumped from 42 to 43 in v0.7.0 I4 — Family::Graph gained
// `memory_replay`; 43 to 44 in v0.7 H4 — Family::Graph gained
// `memory_verify`; 44 to 45 in v0.7 B1 — Family::Core gained
// `memory_load_family`; 45 to 47 in v0.7 K7 — Family::Power gained
// the subscription-reliability pair; 47 to 48 in v0.7 J7 —
// Family::Graph gained `memory_find_paths`; 48 to 49 in v0.7 B2 —
// Family::Core gained `memory_smart_load`; 49 to 50 in v0.7 K8 —
// Family::Power gained `memory_quota_status`.)
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_describe_full_profile_uses_nothing_more_form() {
    let describe = build_capabilities_describe_to_user(&Profile::full());

    // 65 = 66 total - 1 always-on bootstrap excluded from describe.
    // Bumped from 51 to 56 in v0.7.0 L1-5 (Family::Other gained
    // 5 memory_skill_* tools); to 60 with L2-3 (memory_dependents_of_invalidated);
    // to 61 with L2-6 (memory_skill_promote_from_reflection);
    // to 62 with L2-7 (memory_skill_compositional_context); to 63 with QW-1
    // (memory_export_reflection, Family::Power); to 65 with QW-3 follow-up
    // (memory_offload + memory_deref, Family::Power).
    assert!(
        describe.starts_with("I can directly use all 65 memory tools right now ("),
        "full profile describe must open with all-loaded form; got: {describe}"
    );
    assert!(describe.contains("Nothing more to load"));
    assert!(describe.contains("full memory surface is already active"));
    // Closing form omits the on-demand recovery hint (nothing to load).
    assert!(!describe.contains("available on demand"));
}

// ---------------------------------------------------------------------------
// A2: to_describe_to_user on `graph` profile (7 core after v0.7 B1+B2
// + 11 graph after v0.7 J7) lists 18 loaded with a 5-name preview
// ending in ", ..." since there are more loaded than the preview shows.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_describe_graph_profile_uses_preview_ellipsis() {
    let describe = build_capabilities_describe_to_user(&Profile::graph());
    assert!(
        describe.starts_with("I can directly use 18 memory tools right now ("),
        "graph profile describe should open with 18 loaded; got: {describe}"
    );
    // Preview is the first 5 of the 18 loaded — the first 5 core tools.
    assert!(describe.contains("(store, recall, list, get, search, ...)"));
    // 47 more = 65 substantive - 18 loaded. L2-3 + L2-6 + L2-7 each
    // added one tool not loaded under graph (42 → 43 → 44); QW-1 added
    // memory_export_reflection to Family::Power (44 → 45); QW-3 follow-up
    // added memory_offload + memory_deref to Family::Power (45 → 47).
    assert!(describe.contains("47 more"));
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
        None,
        None,
        None,
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
    assert!(
        val.get("to_describe_to_user").is_none(),
        "v2 must not gain the v3 to_describe_to_user field"
    );
    assert!(
        val.get("tools").is_none(),
        "v2 must not gain the v3 tools array"
    );
}

// ---------------------------------------------------------------------------
// A3 matrix cell — allowlist OFF, loaded TRUE → callable_now=TRUE.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a3_allowlist_off_loaded_true_callable_now_true() {
    let tools = build_capabilities_tools(&Profile::core(), None, None);
    let entry = tools
        .iter()
        .find(|t| t.name == "memory_store")
        .expect("memory_store present");
    assert!(entry.loaded, "core profile loads memory_store");
    assert!(
        entry.callable_now,
        "allowlist OFF + loaded TRUE → callable_now must be TRUE; got {entry:?}"
    );
}

// ---------------------------------------------------------------------------
// A3 matrix cell — allowlist OFF, loaded FALSE → callable_now=FALSE.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a3_allowlist_off_loaded_false_callable_now_false() {
    let tools = build_capabilities_tools(&Profile::core(), None, None);
    let entry = tools
        .iter()
        .find(|t| t.name == "memory_kg_query")
        .expect("memory_kg_query present in manifest even when not loaded");
    assert!(!entry.loaded, "core profile does NOT load memory_kg_query");
    assert!(
        !entry.callable_now,
        "allowlist OFF + loaded FALSE → callable_now must be FALSE; got {entry:?}"
    );
}

// ---------------------------------------------------------------------------
// A3 matrix cell — allowlist ON, agent in pattern, loaded TRUE →
// callable_now=TRUE.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a3_allowlist_on_agent_in_pattern_callable_now_true() {
    // Allowlist grants "alice" the core family.
    let cfg = allowlist(&[("alice", &["core"]), ("*", &["core"])]);
    let tools = build_capabilities_tools(&Profile::core(), Some(&cfg), Some("alice"));
    let entry = tools
        .iter()
        .find(|t| t.name == "memory_store")
        .expect("memory_store present");
    assert!(entry.loaded);
    assert!(
        entry.callable_now,
        "allowlist ON + agent in pattern + loaded TRUE → callable_now TRUE; got {entry:?}"
    );
}

// ---------------------------------------------------------------------------
// A3 matrix cell — allowlist ON, agent NOT in pattern, loaded TRUE →
// callable_now=FALSE.
//
// Setup: allowlist grants "alice" the graph family, falls back to "*"
// granting only `core`. Agent "bob" hits the wildcard and asks about
// memory_kg_query (graph family). The graph family is loaded under
// `Profile::full()` (so loaded=TRUE), but the wildcard rule denies bob
// access to graph → callable_now=FALSE.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a3_allowlist_on_agent_denied_callable_now_false() {
    let cfg = allowlist(&[("alice", &["graph"]), ("*", &["core"])]);
    let tools = build_capabilities_tools(&Profile::full(), Some(&cfg), Some("bob"));
    let entry = tools
        .iter()
        .find(|t| t.name == "memory_kg_query")
        .expect("memory_kg_query present");
    assert!(entry.loaded, "full profile loads memory_kg_query");
    assert!(
        !entry.callable_now,
        "allowlist ON + agent NOT in pattern + loaded TRUE → callable_now FALSE; got {entry:?}"
    );

    // Sanity check: the same agent IS allowed core tools per the
    // wildcard, so memory_store should still be callable.
    let core_entry = tools
        .iter()
        .find(|t| t.name == "memory_store")
        .expect("memory_store present");
    assert!(core_entry.loaded);
    assert!(
        core_entry.callable_now,
        "wildcard grants core to bob → memory_store callable_now TRUE"
    );
}

// ---------------------------------------------------------------------------
// A3 — the v3 response surfaces the `tools` array at the top level
// with one entry per registered tool (56 + always-on bootstrap counted
// once = 57, since the bootstrap already lives in Family::Meta).
// (Bumped from 51 to 52 in v0.7.0 Task 4/8 — Family::Power gained
// `memory_reflect`; 52 to 57 in v0.7.0 L1-5 — Family::Other gained
// 5 memory_skill_* tools: memory_skill_register, memory_skill_list,
// memory_skill_get, memory_skill_resource, memory_skill_export.)
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_response_carries_tools_array_with_51_entries() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::full(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    let tools = val["tools"]
        .as_array()
        .expect("top-level tools must be present and an array under v3");
    assert_eq!(
        tools.len(),
        66,
        "v3 must surface all 66 tools regardless of profile (v0.7.0 I4 added \
         memory_replay; v0.7 H4 added memory_verify; v0.7 B1 added \
         memory_load_family; v0.7 B2 added memory_smart_load; v0.7 K7 added \
         memory_subscription_replay + memory_subscription_dlq_list; v0.7 J7 \
         added memory_find_paths; v0.7 K8 added memory_quota_status; v0.7.0 \
         Task 4/8 added memory_reflect; v0.7.0 L2-2 added memory_reflection_origin; \
         v0.7.0 L2-3 added memory_dependents_of_invalidated; \
         v0.7.0 issue #691 added memory_check_agent_action + memory_rule_list; \
         v0.7.0 L1-5 added 5 memory_skill_* tools; v0.7.0 L2-6 added \
         memory_skill_promote_from_reflection; v0.7.0 L2-7 added \
         memory_skill_compositional_context; v0.7.0 QW-1 added \
         memory_export_reflection; v0.7.0 QW-3 follow-up added \
         memory_offload + memory_deref); got {}",
        tools.len()
    );

    // Every entry must have name + family + loaded + callable_now.
    for entry in tools {
        assert!(entry["name"].is_string(), "tool entry needs name: {entry}");
        assert!(entry["family"].is_string(), "tool entry needs family");
        assert!(entry["loaded"].is_boolean(), "tool entry needs loaded bool");
        assert!(
            entry["callable_now"].is_boolean(),
            "tool entry needs callable_now bool"
        );
    }

    // Spot-check that under --profile full + no allowlist, every tool
    // is callable_now.
    for entry in tools {
        assert!(
            entry["callable_now"].as_bool().unwrap(),
            "full profile + no allowlist → every tool callable_now: {entry}"
        );
    }
}

// ---------------------------------------------------------------------------
// A4 case 1 — allowlist disabled (no McpConfig OR empty table) →
// `agent_permitted_families` is OMITTED from the v3 response.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a4_allowlist_disabled_omits_field() {
    // Sub-case A: mcp_config = None.
    assert_eq!(build_agent_permitted_families(None, Some("alice")), None);
    assert_eq!(build_agent_permitted_families(None, None), None);

    // Sub-case B: empty allowlist table → Disabled per the v0.6.4-008
    // contract → omit.
    let cfg = McpConfig {
        profile: None,
        allowlist: Some(HashMap::new()),
    };
    assert_eq!(
        build_agent_permitted_families(Some(&cfg), Some("alice")),
        None,
        "empty allowlist table = disabled = omit field"
    );

    // Sub-case C: full v3 response with allowlist disabled must NOT
    // include the field on the wire (skip_serializing_if).
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        Some("alice"),
        None,
    )
    .expect("v3 capabilities serialize");
    assert!(
        val.get("agent_permitted_families").is_none(),
        "allowlist disabled → field must be absent on wire; got: {val}"
    );
}

// ---------------------------------------------------------------------------
// A4 case 2 — allowlist enabled with agent → field carries the family
// names the agent is permitted to access.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a4_allowlist_with_agent_lists_families() {
    let cfg = allowlist(&[
        ("alice", &["core", "graph"]),
        ("bob", &["core"]),
        ("*", &["core"]),
    ]);
    // alice → core + graph
    let alice = build_agent_permitted_families(Some(&cfg), Some("alice")).unwrap();
    assert_eq!(alice, vec!["core".to_string(), "graph".to_string()]);

    // bob → core only (his explicit row wins over the wildcard)
    let bob = build_agent_permitted_families(Some(&cfg), Some("bob")).unwrap();
    assert_eq!(bob, vec!["core".to_string()]);

    // unknown agent → wildcard fallback (core only)
    let unknown = build_agent_permitted_families(Some(&cfg), Some("eve")).unwrap();
    assert_eq!(unknown, vec!["core".to_string()]);

    // The field round-trips on the wire.
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::full(),
        Some(&cfg),
        Some("alice"),
        None,
    )
    .expect("v3 capabilities serialize");
    let permitted = val["agent_permitted_families"]
        .as_array()
        .expect("agent_permitted_families must be present when allowlist enabled + agent_id given");
    let names: Vec<&str> = permitted.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(names, vec!["core", "graph"]);
}

// ---------------------------------------------------------------------------
// A4 case 3 — allowlist enabled but no agent_id → field omitted (the
// v0.6.4-008 default for an unknown caller is restrictive, but A4's
// contract is "tell the caller what they're allowed only when the
// caller identified themselves" — present absence is the signal).
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_a4_allowlist_no_agent_id_omits_field() {
    let cfg = allowlist(&[("alice", &["core"]), ("*", &["core"])]);
    assert_eq!(build_agent_permitted_families(Some(&cfg), None), None);

    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        Some(&cfg),
        None, // no agent_id
        None,
    )
    .expect("v3 capabilities serialize");
    assert!(
        val.get("agent_permitted_families").is_none(),
        "no agent_id → field must be absent even with allowlist enabled; got: {val}"
    );
}

// ---------------------------------------------------------------------------
// B4 case 1 — when the detected harness supports deferred-tool
// registration (Claude Code today), the v3 response carries
// `your_harness_supports_deferred_registration: true` so the LLM can
// reason about whether B1's `memory_load_family` will actually surface
// new tools mid-session.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_b4_claude_code_harness_advertises_deferred_true() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let harness = Harness::ClaudeCode;
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        Some(&harness),
    )
    .expect("v3 capabilities serialize");
    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "Claude Code → field must be present and true; got: {val}"
    );
}

// ---------------------------------------------------------------------------
// B4 case 2 — when the detected harness does NOT support deferred
// registration (Codex today), the field is present but false. Presence
// is the signal that the substrate did detect a harness; the value
// tells the LLM that mid-session loading won't surface new tools.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_b4_codex_harness_advertises_deferred_false() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let harness = Harness::Codex;
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        Some(&harness),
    )
    .expect("v3 capabilities serialize");
    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "Codex → field must be present and false; got: {val}"
    );
}

// ---------------------------------------------------------------------------
// B4 case 3 — when no clientInfo was captured (HTTP callers, or an MCP
// session that issued `memory_capabilities` before `initialize`), the
// field is OMITTED from the wire entirely. Absence carries meaning
// distinct from `false`: false means "we know your harness can't",
// absent means "we don't know your harness".
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_b4_no_harness_omits_field_from_wire() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None, // no harness detected
    )
    .expect("v3 capabilities serialize");
    assert!(
        val.get("your_harness_supports_deferred_registration")
            .is_none(),
        "no harness → field must be absent on wire (skip_serializing_if); got: {val}"
    );
}

// ---------------------------------------------------------------------------
// B4 case 4 — unknown harness (Generic) defaults to false. Conservative
// because we'd rather under-promise mid-session loading than have an
// LLM tell an end-user "I just loaded the graph tools" and have those
// tools never appear because the harness cached the manifest.
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_b4_generic_harness_defaults_deferred_false() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let harness = Harness::Generic("some-unknown-mcp-client".to_string());
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        Some(&harness),
    )
    .expect("v3 capabilities serialize");
    assert_eq!(
        val.get("your_harness_supports_deferred_registration")
            .and_then(serde_json::Value::as_bool),
        Some(false),
        "unknown harness → field must be present and false (conservative default); got: {val}"
    );
}

// ---------------------------------------------------------------------------
// B4 case 5 — the v2 wire shape is unaffected by B4. v2 callers must
// not gain the field even when a harness is in scope (the field lives
// on `CapabilitiesV3` only).
// ---------------------------------------------------------------------------
#[test]
fn cap_v3_b4_v2_callers_unaffected() {
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
    assert!(
        val.get("your_harness_supports_deferred_registration")
            .is_none(),
        "v2 must not gain the B4 field"
    );
}

// ---------------------------------------------------------------------------
// K5 — `permissions.rule_summary` is populated from the live governance
// configuration: ordered (lex) one-line summaries, one per active
// policy. The field is omitted from the wire when no policies exist
// (skip_serializing_if = "Vec::is_empty") so v2 callers continue to see
// the v0.6.3.1 honesty disclosure shape, and v3 callers gain a real
// per-rule serializer (closes the K5 spec).
// ---------------------------------------------------------------------------

/// Helper: seed a namespace standard memory with an explicit
/// `metadata.governance` policy attached. Mirrors the production path
/// (`db::insert` + `db::set_namespace_standard`) used by the
/// ship-gate scenarios so the capabilities surface walks the same
/// rows the gate would.
fn seed_governance_policy(
    conn: &rusqlite::Connection,
    namespace: &str,
    policy: &ai_memory::models::GovernancePolicy,
) {
    use ai_memory::models::{Memory, Tier, default_metadata};
    let now = chrono::Utc::now().to_rfc3339();
    let mut metadata = default_metadata();
    if let Some(obj) = metadata.as_object_mut() {
        obj.insert(
            "agent_id".to_string(),
            serde_json::Value::String("test-owner".to_string()),
        );
        obj.insert(
            "governance".to_string(),
            serde_json::to_value(policy).unwrap(),
        );
    }
    let standard = Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Long,
        namespace: format!("_standards-{namespace}"),
        title: format!("standard for {namespace}"),
        content: "policy".to_string(),
        tags: vec![],
        priority: 9,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata,
        reflection_depth: 0,
        memory_kind: ai_memory::models::MemoryKind::Observation,
    };
    let standard_id = ai_memory::db::insert(conn, &standard).unwrap();
    ai_memory::db::set_namespace_standard(conn, namespace, &standard_id, None).unwrap();
}

/// K5 — empty governance state: `permissions.rule_summary` is absent
/// from the wire (`skip_serializing_if = "Vec::is_empty"`). This is the
/// v0.6.3.1 honesty-disclosure shape preserved when no policies are
/// configured.
#[test]
fn cap_v3_k5_rule_summary_empty_state_omits_field() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    assert!(
        val["permissions"].get("rule_summary").is_none(),
        "K5: empty governance state must omit `rule_summary` from the wire \
         (skip_serializing_if = Vec::is_empty); got: {val}"
    );
    // `active_rules` still surfaces (it's a count, not a per-rule list)
    // and must be zero in the empty state.
    assert_eq!(val["permissions"]["active_rules"], 0);
}

/// K5 — single policy at `team`: `rule_summary` carries one entry
/// naming the namespace plus all five policy levels. Pins the wire
/// format an LLM/operator can parse without an extra round-trip.
#[test]
fn cap_v3_k5_rule_summary_single_policy_carries_one_entry() {
    use ai_memory::models::{ApproverType, GovernanceLevel, GovernancePolicy};

    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let policy = GovernancePolicy {
        write: GovernanceLevel::Approve,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: None,
        auto_export_reflections_to_filesystem: None,
    };
    seed_governance_policy(&conn, "team", &policy);

    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    let arr = val["permissions"]["rule_summary"]
        .as_array()
        .expect("rule_summary must be present and an array under K5; got missing/non-array");
    assert_eq!(
        arr.len(),
        1,
        "K5: single policy must surface as exactly one rule_summary entry; got: {arr:?}"
    );
    let line = arr[0].as_str().expect("entry is a stringy summary");
    assert!(
        line.starts_with("team — "),
        "K5: rule_summary entry must lead with the namespace; got: {line}"
    );
    assert!(
        line.contains("write=approve"),
        "missing write=; got: {line}"
    );
    assert!(
        line.contains("promote=any"),
        "missing promote=; got: {line}"
    );
    assert!(
        line.contains("delete=owner"),
        "missing delete=; got: {line}"
    );
    assert!(
        line.contains("approver=human"),
        "missing approver=; got: {line}"
    );
    assert!(
        line.contains("inherit=true"),
        "missing inherit=; got: {line}"
    );
    // `active_rules` count must agree with the per-rule list length.
    assert_eq!(val["permissions"]["active_rules"], 1);
}

/// K5 — multiple policies: `rule_summary` is sorted lexicographically
/// by namespace. The DB layer returns rows already ORDER BY-sorted, so
/// this test pins the contract end-to-end through the capabilities
/// builder.
#[test]
fn cap_v3_k5_rule_summary_multiple_policies_lex_ordered() {
    use ai_memory::models::{ApproverType, GovernanceLevel, GovernancePolicy};

    let tier_config = semantic_tier();
    let conn = fresh_conn();
    // Seed in deliberately non-lex order so a buggy implementation
    // that preserves insertion order would fail this test.
    let zeta = GovernancePolicy {
        write: GovernanceLevel::Owner,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Agent("maintainer".to_string()),
        inherit: false,
        max_reflection_depth: None,
        auto_export_reflections_to_filesystem: None,
    };
    let alpha = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Approve,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Consensus(3),
        inherit: true,
        max_reflection_depth: None,
        auto_export_reflections_to_filesystem: None,
    };
    let middle = GovernancePolicy::default();
    seed_governance_policy(&conn, "zeta", &zeta);
    seed_governance_policy(&conn, "alpha", &alpha);
    seed_governance_policy(&conn, "middle", &middle);

    let val = handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        &Profile::core(),
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize");

    let arr = val["permissions"]["rule_summary"]
        .as_array()
        .expect("rule_summary must be present and an array under K5");
    assert_eq!(arr.len(), 3, "expected 3 rule entries; got: {arr:?}");

    // Lex order: alpha < middle < zeta.
    let lines: Vec<&str> = arr.iter().map(|v| v.as_str().unwrap()).collect();
    assert!(
        lines[0].starts_with("alpha — "),
        "first entry must be alpha; got: {lines:?}"
    );
    assert!(
        lines[1].starts_with("middle — "),
        "second entry must be middle; got: {lines:?}"
    );
    assert!(
        lines[2].starts_with("zeta — "),
        "third entry must be zeta; got: {lines:?}"
    );

    // Spot-check the discriminator-tagged approver rendering — the
    // `Consensus(N)` and `Agent(id)` variants must surface their inner
    // value so an operator can tell them apart from `Human`.
    assert!(
        lines[0].contains("approver=consensus:3"),
        "Consensus approver must render with the count; got: {}",
        lines[0]
    );
    assert!(
        lines[2].contains("approver=agent:maintainer"),
        "Agent approver must render with the id; got: {}",
        lines[2]
    );
    // `inherit=false` must round-trip so the no-inherit override is
    // visible at a glance.
    assert!(
        lines[2].contains("inherit=false"),
        "inherit=false override must surface; got: {}",
        lines[2]
    );

    // Active count agrees.
    assert_eq!(val["permissions"]["active_rules"], 3);
}

/// K5 — v2 callers continue to see the field omitted from the wire
/// when no policies are configured (the v0.6.3.1 honesty-disclosure
/// shape). When policies *are* configured, v2 surfaces the same field
/// (the permissions block is shared between v2 + v3) — the contract
/// is "no drift in the empty case", not "v2 hides it forever".
#[test]
fn cap_v3_k5_v2_callers_see_omitted_field_when_empty() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        false,
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");
    assert!(
        val["permissions"].get("rule_summary").is_none(),
        "K5: v2 wire shape must keep the empty-state honesty disclosure \
         (rule_summary omitted when no policies); got: {val}"
    );
}
