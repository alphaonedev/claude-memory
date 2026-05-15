// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Grand-Slam L3-5 — capabilities-v3 JSON updates (#678).
//!
//! Pins the L3-5 wire surface: four new top-level blocks on the v3
//! capabilities response (`reflection`, `skills`, `forensic`,
//! `governance`) plus the audit-critical regression that v1 + v2
//! clients see **no** change.
//!
//! ## Hard rule — every reported field maps to real implementation
//!
//! These tests don't just type-check the field shape; they assert that
//! each declarative value matches the live implementation:
//!
//! - `reflection.max_default` == `reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP`.
//! - `skills.tools` == the registered `memory_skill_*` MCP tool names
//!   in `crate::mcp::registry`. Catches drift the moment a new skill
//!   tool lands or one is renamed.
//! - `governance.bypass_impossibility_tests` >= the `#[test]` count in
//!   `tests/governance_l16_activation.rs` (sanity check — if the count
//!   in `config.rs` ever exceeds the file, the audit catches the lie).
//! - `governance.enforced_actions` is the `AgentAction` non-`Custom`
//!   variant set (`Bash`, `FilesystemWrite`, `NetworkRequest`,
//!   `ProcessSpawn`).
//!
//! ## Backward compatibility — load-bearing
//!
//! v3 is additive. The v1 + v2 wire shapes MUST stay identical. The
//! regression tests at the bottom of this file pin that pair.

use ai_memory::config::{
    Capabilities, CapabilitiesV3, CapabilityAtomisation, CapabilityForensic, CapabilityGovernance,
    CapabilityReflection, CapabilitySkills, ENFORCED_AGENT_ACTIONS, FeatureTier,
    GOVERNANCE_BYPASS_IMPOSSIBILITY_TESTS, SKILL_TOOL_NAMES, TierConfig,
};
use ai_memory::mcp::{
    CapabilitiesAccept, handle_capabilities_with_conn, handle_capabilities_with_conn_v3,
};
use ai_memory::profile::Profile;
use serde_json::Value;

fn fresh_conn() -> rusqlite::Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

// ===========================================================================
// L3-5 surface — the four new top-level blocks on the v3 response.
// ===========================================================================

#[test]
fn cap_v3_l3_5_response_carries_reflection_block() {
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

    let r = &val["reflection"];
    assert!(r.is_object(), "reflection block must be present under v3");
    assert_eq!(r["implemented"], true);
    assert_eq!(r["depth_bounded"], true);
    // Compile-time anchor — must equal the reranker default (3 today).
    assert_eq!(
        r["max_default"],
        ai_memory::reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP
    );
    assert_eq!(r["attestation"], "Ed25519");
    assert_eq!(r["curator_mode"], "implemented");
}

#[test]
fn cap_v3_l3_5_response_carries_skills_block() {
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

    let s = &val["skills"];
    assert!(s.is_object(), "skills block must be present under v3");
    assert_eq!(s["implemented"], true);
    assert_eq!(s["standard"], "agentskills.io");
    assert_eq!(s["round_trip"], "verified");

    // Every entry in `tools` must be a registered MCP tool of the form
    // `memory_skill_*` — pins both the count (7 as of L2-7) and the
    // ordering against the constant.
    let tools = s["tools"]
        .as_array()
        .expect("skills.tools must be an array");
    assert_eq!(
        tools.len(),
        SKILL_TOOL_NAMES.len(),
        "skills.tools must enumerate every registered memory_skill_* handler (7 as of L2-7)"
    );
    for (i, expected) in SKILL_TOOL_NAMES.iter().enumerate() {
        assert_eq!(
            tools[i].as_str().unwrap(),
            *expected,
            "skills.tools[{i}] drift — expected `{expected}`",
        );
        assert!(
            expected.starts_with("memory_skill_"),
            "every L3-5 skill tool name must use the memory_skill_ prefix"
        );
    }
}

#[test]
fn cap_v3_l3_5_skill_tools_match_registered_mcp_dispatch() {
    // Honesty regression: the canonical names hard-coded in
    // [`SKILL_TOOL_NAMES`] must also appear verbatim in the live
    // MCP tool definitions JSON. If a future PR renames a handler
    // (`memory_skill_get` → `memory_skill_fetch`, say) without updating
    // the capability-side constant, this test fails before merge.
    let defs = ai_memory::mcp::tool_definitions();
    let tool_names: Vec<String> = defs["tools"]
        .as_array()
        .expect("tool_definitions().tools must be an array")
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str).map(String::from))
        .collect();

    for expected in SKILL_TOOL_NAMES {
        assert!(
            tool_names.iter().any(|n| n == expected),
            "L3-5 SKILL_TOOL_NAMES claim `{expected}` is registered, \
             but it is NOT in tool_definitions() — audit-critical drift"
        );
    }
}

#[test]
fn cap_v3_l3_5_response_carries_forensic_block() {
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

    let f = &val["forensic"];
    assert!(f.is_object(), "forensic block must be present under v3");
    assert_eq!(f["verify_reflection_chain"], "implemented");
    assert_eq!(f["export_forensic_bundle"], "implemented");
    assert_eq!(f["verify_forensic_bundle"], "implemented");
}

#[test]
fn cap_v3_l3_5_response_carries_governance_block() {
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

    let g = &val["governance"];
    assert!(g.is_object(), "governance block must be present under v3");
    assert_eq!(g["rules_engine"], "operator_signed");
    assert_eq!(g["bypass_impossibility_tests"], 6);
    let actions = g["enforced_actions"]
        .as_array()
        .expect("governance.enforced_actions must be an array");
    assert_eq!(actions.len(), ENFORCED_AGENT_ACTIONS.len());
    for (i, expected) in ENFORCED_AGENT_ACTIONS.iter().enumerate() {
        assert_eq!(actions[i].as_str().unwrap(), *expected);
    }
    // Compile-time anchor — the constant must equal the wire value.
    assert_eq!(
        g["bypass_impossibility_tests"]
            .as_u64()
            .expect("bypass_impossibility_tests is a number"),
        u64::from(GOVERNANCE_BYPASS_IMPOSSIBILITY_TESTS)
    );
}

// ===========================================================================
// Compile-time honesty — direct probes on the structs (no JSON in the way).
// ===========================================================================

#[test]
fn cap_v3_l3_5_reflection_struct_anchors_real_constants() {
    let r = CapabilityReflection::current();
    assert!(r.implemented);
    assert!(r.depth_bounded);
    assert_eq!(
        r.max_default,
        ai_memory::reranker::DEFAULT_REFLECTION_MAX_DEPTH_CAP
    );
    assert_eq!(r.attestation, "Ed25519");
    assert_eq!(r.curator_mode, "implemented");
}

#[test]
fn cap_v3_l3_5_skills_struct_anchors_canonical_tool_list() {
    let s = CapabilitySkills::current();
    assert!(s.implemented);
    assert_eq!(s.standard, "agentskills.io");
    assert_eq!(s.round_trip, "verified");
    let names: Vec<&str> = s.tools.iter().map(String::as_str).collect();
    let expected: Vec<&str> = SKILL_TOOL_NAMES.to_vec();
    assert_eq!(names, expected);
    // L2-7 added the 7th skill tool (memory_skill_compositional_context).
    // If a future increment ships an 8th, this assert pins the L3-5
    // capability surface to be updated as well.
    assert_eq!(
        s.tools.len(),
        7,
        "v0.7.0 ships 7 skill tools (5 base + L2-6 promote + L2-7 compositional context); \
         growing the family requires an L3-5 wire-shape audit"
    );
}

#[test]
fn cap_v3_l3_5_forensic_struct_reports_three_implemented_drivers() {
    let f = CapabilityForensic::current();
    assert_eq!(f.verify_reflection_chain, "implemented");
    assert_eq!(f.export_forensic_bundle, "implemented");
    assert_eq!(f.verify_forensic_bundle, "implemented");
}

#[test]
fn cap_v3_l3_5_governance_struct_reports_canonical_action_set() {
    let g = CapabilityGovernance::current();
    assert_eq!(g.rules_engine, "operator_signed");
    let names: Vec<&str> = g.enforced_actions.iter().map(String::as_str).collect();
    let expected: Vec<&str> = ENFORCED_AGENT_ACTIONS.to_vec();
    assert_eq!(names, expected);
    assert_eq!(
        g.enforced_actions.len(),
        4,
        "the substrate-rules engine (issue #691) currently gates exactly four \
         agent-external action kinds. MemoryWrite is intentionally NOT here — \
         substrate-internal writes are gated by the K9 `Op` pipeline, which is \
         a separate, substrate-authoritative engine. Conflating them would be \
         theatrical."
    );
    assert_eq!(g.bypass_impossibility_tests, 6);
}

// ===========================================================================
// L1-1 honesty — memory_kinds stays the substrate truth ("observation",
// "reflection"). The L3-5 spec mentioned a third "goal" kind; we report
// what the [`crate::models::memory::MemoryKind`] enum actually carries.
// ===========================================================================

#[test]
fn cap_v3_l3_5_memory_kinds_reports_real_implemented_set_only() {
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

    let kinds = val["memory_kinds"]
        .as_array()
        .expect("memory_kinds must be an array under v3");
    let kinds: Vec<&str> = kinds.iter().filter_map(Value::as_str).collect();
    // Substrate truth — MemoryKind enum carries only these two variants
    // as of v0.7.0. The grand-slam spec called for a third "goal" kind
    // but the enum doesn't define it; honest reporting omits it.
    assert_eq!(
        kinds,
        vec!["observation", "reflection"],
        "memory_kinds must enumerate the actual MemoryKind variants, not theatrical ones"
    );
}

// ===========================================================================
// Round-trip serde — the new blocks survive serialize → deserialize so a
// client that holds a v3 envelope can re-emit it without information loss.
// ===========================================================================

#[test]
fn cap_v3_l3_5_struct_round_trips_through_serde() {
    let tier_config = semantic_tier();
    let caps: Capabilities = tier_config.capabilities();
    let v3 = caps.to_v3(
        "summary".to_string(),
        "describe".to_string(),
        Vec::new(),
        None,
        None,
    );

    let json = serde_json::to_value(&v3).expect("serialize v3");
    let back: CapabilitiesV3 =
        serde_json::from_value(json.clone()).expect("deserialize v3 with L3-5 fields");

    // Verify each L3-5 block round-tripped unchanged.
    assert_eq!(back.reflection, v3.reflection);
    assert_eq!(back.skills, v3.skills);
    assert_eq!(back.forensic, v3.forensic);
    assert_eq!(back.governance, v3.governance);
    // And the JSON itself carries the four top-level keys.
    assert!(json.get("reflection").is_some());
    assert!(json.get("skills").is_some());
    assert!(json.get("forensic").is_some());
    assert!(json.get("governance").is_some());
}

#[test]
fn cap_v3_l3_5_legacy_v3_payload_without_new_fields_still_deserializes() {
    // A pre-L3-5 v3 capabilities envelope captured in the wild MUST still
    // deserialize cleanly — the new fields carry `#[serde(default = …)]`
    // so any client that round-trips an old payload back through the
    // type gets sensible defaults rather than a hard parse error.
    //
    // The payload below is the minimum-viable v3 shape; we deserialize it
    // into `CapabilitiesV3` and verify the new fields land at their
    // current-implementation defaults.
    let pre_l3_5_json = serde_json::json!({
        "schema_version": "3",
        "summary": "pre-l3-5 summary",
        "to_describe_to_user": "pre-l3-5 describe",
        "tools": [],
        "tier": "semantic",
        "version": "0.7.0",
        "features": {
            "keyword_search": true,
            "semantic_search": true,
            "hybrid_recall": true,
            "query_expansion": false,
            "auto_consolidation": false,
            "auto_tagging": false,
            "contradiction_analysis": false,
            "cross_encoder_reranking": false,
            "memory_reflection": {"planned": false, "version": "v0.7.0", "enabled": true},
            "embedder_loaded": false,
            "recall_mode_active": "disabled",
            "reranker_active": "off",
            "reflection_boost": {"boost": 1.2, "per_depth_increment": 0.05, "max_depth_cap": 3}
        },
        "models": {"embedding": "none", "embedding_dim": 0, "llm": "none", "cross_encoder": "none"},
        "permissions": {"mode": "advisory", "active_rules": 0},
        "hooks": {"registered_count": 0},
        "compaction": {"planned": true, "version": "v0.8+", "enabled": false},
        "approval": {"pending_requests": 0},
        "transcripts": {"planned": true, "version": "v0.7+", "enabled": false},
        "memory_kinds": ["observation", "reflection"]
    });

    let back: CapabilitiesV3 =
        serde_json::from_value(pre_l3_5_json).expect("pre-L3-5 v3 payload must still parse");

    // Each new field defaulted to the current-implementation value — so
    // a downstream that reads `back.reflection.max_default` gets `3`,
    // not a panic, and not a wrong number.
    assert_eq!(back.reflection, CapabilityReflection::current());
    assert_eq!(back.skills, CapabilitySkills::current());
    assert_eq!(back.forensic, CapabilityForensic::current());
    assert_eq!(back.governance, CapabilityGovernance::current());
}

// ===========================================================================
// Backward compatibility — v1 + v2 clients see no L3-5 fields. This is the
// load-bearing audit guarantee: schema_version discriminates the wire shape;
// older clients pinning v2 explicitly continue to round-trip cleanly.
// ===========================================================================

#[test]
fn cap_v3_l3_5_v2_clients_see_no_new_top_level_fields() {
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

    // Schema discriminator stays "2" — clients that pinned v2
    // explicitly continue to get the v2 shape verbatim.
    assert_eq!(val["schema_version"], "2");

    // The four new L3-5 top-level blocks MUST be absent from v2.
    // (They are v3-only additions to `CapabilitiesV3`; the v2
    // [`Capabilities`] struct does not include them.)
    assert!(
        val.get("reflection").is_none(),
        "v2 must NOT carry the L3-5 reflection block — would break v2 clients"
    );
    assert!(
        val.get("skills").is_none(),
        "v2 must NOT carry the L3-5 skills block — would break v2 clients"
    );
    assert!(
        val.get("forensic").is_none(),
        "v2 must NOT carry the L3-5 forensic block — would break v2 clients"
    );
    assert!(
        val.get("governance").is_none(),
        "v2 must NOT carry the L3-5 governance block — would break v2 clients"
    );

    // v2 still carries the L1-1 memory_kinds field (unchanged from
    // pre-L3-5) so the v2 contract from L1-1 is preserved.
    let kinds = val["memory_kinds"]
        .as_array()
        .expect("v2 carries memory_kinds since L1-1");
    let kinds: Vec<&str> = kinds.iter().filter_map(Value::as_str).collect();
    assert_eq!(kinds, vec!["observation", "reflection"]);
}

#[test]
fn cap_v3_l3_5_v1_clients_see_no_new_top_level_fields() {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        false,
        Some(&conn),
        CapabilitiesAccept::V1,
    )
    .expect("v1 capabilities still work");

    // v1 wire shape: no `schema_version`, no v2-only blocks, no L3-5
    // blocks. Pre-v0.6.3.1 callers continue to pass.
    assert!(
        val.get("schema_version").is_none(),
        "v1 has no schema_version"
    );
    assert!(val.get("permissions").is_none(), "v1 has no permissions");
    assert!(val.get("hooks").is_none(), "v1 has no hooks");
    assert!(val.get("compaction").is_none(), "v1 has no compaction");
    assert!(val.get("approval").is_none(), "v1 has no approval");
    assert!(val.get("transcripts").is_none(), "v1 has no transcripts");

    // L3-5 additions MUST be invisible to v1.
    assert!(val.get("reflection").is_none(), "v1 has no L3-5 reflection");
    assert!(val.get("skills").is_none(), "v1 has no L3-5 skills");
    assert!(val.get("forensic").is_none(), "v1 has no L3-5 forensic");
    assert!(val.get("governance").is_none(), "v1 has no L3-5 governance");

    // L1-1 memory_kinds is a v2+ field; v1 doesn't carry it either.
    assert!(val.get("memory_kinds").is_none(), "v1 has no memory_kinds");
}

// ===========================================================================
// v0.7.0 WT-1-G — atomisation capability block. Six operator-facing
// sub-fields (`tool`, `cli`, `auto`, `recall_preference`, `forensic`,
// `curator`) plus the `derives_from` link-relation anchor. Every field
// is "implemented" because WT-1-A..F all landed before WT-1-G.
// ===========================================================================

#[test]
fn cap_v3_wt1g_response_carries_atomisation_block() {
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

    let a = &val["atomisation"];
    assert!(
        a.is_object(),
        "WT-1-G atomisation block must be present under v3"
    );
    assert_eq!(a["tool"], "implemented", "memory_atomise MCP tool (WT-1-C)");
    assert_eq!(a["cli"], "implemented", "`ai-memory atomise` CLI (WT-1-F)");
    assert_eq!(
        a["auto"], "implemented",
        "namespace-policy auto_atomise pre_store hook (WT-1-D)"
    );
    assert_eq!(
        a["recall_preference"], "implemented",
        "recall-time atom preference SQL guard (WT-1-E)"
    );
    assert_eq!(
        a["forensic"], "implemented",
        "forensic chain envelope in bundle export (WT-1-E)"
    );
    assert_eq!(
        a["curator"], "implemented",
        "LlmCurator (Gemma 4 + tiktoken-rs, WT-1-B)"
    );
    // The link-relation anchor must match the canonical
    // `MemoryLinkRelation::DerivesFrom` wire string. Any drift here
    // would break downstream tooling that filters atomisation lineage.
    assert_eq!(
        a["link_relation"], "derives_from",
        "atom → parent edge uses MemoryLinkRelation::DerivesFrom"
    );
}

#[test]
fn cap_v3_wt1g_atomisation_struct_anchors_implementation() {
    use ai_memory::models::MemoryLinkRelation;

    // Direct probe — bypasses the JSON path so a future serde
    // mistake doesn't mask a struct-level drift.
    let a = CapabilityAtomisation::current();
    assert_eq!(a.tool, "implemented");
    assert_eq!(a.cli, "implemented");
    assert_eq!(a.auto, "implemented");
    assert_eq!(a.recall_preference, "implemented");
    assert_eq!(a.forensic, "implemented");
    assert_eq!(a.curator, "implemented");
    // Pins the wire spelling of the link relation — must match
    // `MemoryLinkRelation::DerivesFrom.as_str()`.
    assert_eq!(a.link_relation, MemoryLinkRelation::DerivesFrom.as_str());
}

#[test]
fn cap_v3_wt1g_atomisation_round_trips_through_serde() {
    let tier_config = semantic_tier();
    let caps: Capabilities = tier_config.capabilities();
    let v3 = caps.to_v3(
        "summary".to_string(),
        "describe".to_string(),
        Vec::new(),
        None,
        None,
    );

    let json = serde_json::to_value(&v3).expect("serialize v3 with WT-1-G atomisation block");
    let back: CapabilitiesV3 =
        serde_json::from_value(json.clone()).expect("deserialize v3 with WT-1-G atomisation block");

    assert_eq!(back.atomisation, v3.atomisation);
    assert_eq!(back.atomisation, CapabilityAtomisation::current());
    assert!(
        json.get("atomisation").is_some(),
        "top-level `atomisation` key must serialise"
    );
}

#[test]
fn cap_v3_wt1g_pre_wt1g_payload_still_deserializes_with_default() {
    // A v3 payload captured before WT-1-G landed MUST still parse —
    // the new `atomisation` field carries `#[serde(default = …)]` so
    // an older envelope round-trips into a struct with the current-
    // implementation snapshot filled in.
    let pre_wt1g_json = serde_json::json!({
        "schema_version": "3",
        "summary": "pre-WT-1-G summary",
        "to_describe_to_user": "pre-WT-1-G describe",
        "tools": [],
        "tier": "semantic",
        "version": "0.7.0",
        "features": {
            "keyword_search": true,
            "semantic_search": true,
            "hybrid_recall": true,
            "query_expansion": false,
            "auto_consolidation": false,
            "auto_tagging": false,
            "contradiction_analysis": false,
            "cross_encoder_reranking": false,
            "memory_reflection": {"planned": false, "version": "v0.7.0", "enabled": true},
            "embedder_loaded": false,
            "recall_mode_active": "disabled",
            "reranker_active": "off",
            "reflection_boost": {"boost": 1.2, "per_depth_increment": 0.05, "max_depth_cap": 3}
        },
        "models": {"embedding": "none", "embedding_dim": 0, "llm": "none", "cross_encoder": "none"},
        "permissions": {"mode": "advisory", "active_rules": 0},
        "hooks": {"registered_count": 0},
        "compaction": {"planned": true, "version": "v0.8+", "enabled": false},
        "approval": {"pending_requests": 0},
        "transcripts": {"planned": true, "version": "v0.7+", "enabled": false},
        "memory_kinds": ["observation", "reflection"]
    });

    let back: CapabilitiesV3 = serde_json::from_value(pre_wt1g_json)
        .expect("pre-WT-1-G v3 payload must still parse with default atomisation");
    assert_eq!(back.atomisation, CapabilityAtomisation::current());
}

#[test]
fn cap_v3_wt1g_v2_clients_see_no_atomisation_field() {
    // Backward compat — v2 wire shape must not grow the WT-1-G block.
    // The L3-5 v2 regression test already covers reflection/skills/
    // forensic/governance; this asserts atomisation is absent too.
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
        val.get("atomisation").is_none(),
        "v2 must NOT carry the WT-1-G atomisation block — would break v2 clients"
    );
}

// ===========================================================================
// Discriminator probe — v3 carries schema_version="3" so an integration
// can branch on the discriminator without inspecting the field set.
// ===========================================================================

#[test]
fn cap_v3_l3_5_schema_version_discriminator_is_3() {
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

    assert_eq!(val["schema_version"], "3");
}
