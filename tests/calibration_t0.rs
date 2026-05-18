// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

#![allow(clippy::doc_lazy_continuation)]

//! Discovery Gate **T0 calibration cells** — assert canonical phrasing
//! present in capabilities-v3 responses across all named profiles.
//!
//! v0.7.0 A2 (`to_describe_to_user`) is the user-facing sentence the
//! NHI Discovery Gate expects every reasoning-class LLM to reproduce
//! when asked "what tools do you have?". This test file is the
//! corresponding T0 calibration cell that runs in CI: it pins the
//! canonical strings from `docs/v0.7/canonical-phrasings.md` so any
//! drift in the substrate breaks the build before it reaches a
//! Discovery Gate observation cell.
//!
//! When a phrasing changes intentionally (e.g., a future increment
//! adds a new recovery path), update both:
//! 1. `docs/v0.7/canonical-phrasings.md` (the human-readable spec)
//! 2. `src/mcp.rs::build_capabilities_{summary,describe_to_user}`
//!    (the substrate)
//!
//! …and re-run this test. Drift between the spec and the substrate is
//! exactly what this file is designed to surface.

use ai_memory::config::{FeatureTier, TierConfig};
use ai_memory::mcp::handle_capabilities_with_conn_v3;
use ai_memory::profile::Profile;
use serde_json::Value;

mod common;
use common::fresh_conn;

fn semantic_tier() -> TierConfig {
    FeatureTier::Semantic.config()
}

fn v3_response(profile: &Profile) -> Value {
    let tier_config = semantic_tier();
    let conn = fresh_conn();
    handle_capabilities_with_conn_v3(
        &tier_config,
        None,
        false,
        Some(&conn),
        profile,
        None,
        None,
        None,
    )
    .expect("v3 capabilities serialize")
}

// ---------------------------------------------------------------------------
// T0-A2-CORE — `to_describe_to_user` on `--profile core` matches the
// canonical phrasing pinned in docs/v0.7/canonical-phrasings.md verbatim.
// ---------------------------------------------------------------------------
#[test]
fn t0_describe_to_user_core_profile_canonical_phrasing() {
    let val = v3_response(&Profile::core());
    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");

    // 43 = 50 user-relevant tools − 7 core. (50 = 51 total tools − 1
    // always-on bootstrap.) The bootstrap (`memory_capabilities`) is
    // excluded from BOTH the loaded and the unloaded count in
    // `to_describe_to_user` (it's plumbing, not a feature). Total
    // bumped from 43 to 44 in v0.7.0 I4 — Family::Graph gained
    // `memory_replay`; to 45 in v0.7 H4 — Family::Graph gained
    // `memory_verify`; to 46 in v0.7 B1 — Family::Core gained
    // `memory_load_family`; to 48 in v0.7 K7 — Family::Power gained
    // `memory_subscription_replay` + `memory_subscription_dlq_list`;
    // to 49 in v0.7 J7 — Family::Graph gained `memory_find_paths`;
    // to 50 in v0.7 B2 — Family::Core gained `memory_smart_load`;
    // to 51 in v0.7 K8 — Family::Power gained `memory_quota_status`;
    // to 52 in v0.7.0 Task 4/8 (#655) — Family::Power gained `memory_reflect`;
    // to 56 in v0.7.0 L1-5 — Family::Other gained 5 memory_skill_* tools.
    // Loaded under core bumped from 5 to 6 with B1 then to 7 with B2,
    // so the preview now overflows the 5-name cap (ends in ", ...").
    // memory_reflect lives in Family::Power, so it grows the "more"
    // bucket from 43 to 44 without changing the loaded count of 7.
    // v0.7.0 L2-2 (S6-M1) — Family::Power gained
    // `memory_reflection_origin` (53 total). Not loaded under core, so
    // the "more" bucket grows from 44 to 45.
    // v0.7.0 (issue #691) — Family::Power gained
    // `memory_check_agent_action` + `memory_rule_list` (not loaded
    // under core), so the "more" count grows from 45 to 47.
    // v0.7.0 L1-5 — Family::Other gained 5 memory_skill_* tools (not
    // loaded under core), so the "more" count grows from 47 to 52.
    // v0.7.0 L2-3 (issue #668) — Family::Power gained
    // `memory_dependents_of_invalidated` (not loaded under core), so
    // the "more" count grows from 52 to 53.
    // v0.7.0 L2-6 (issue #671) — Family::Other gained
    // `memory_skill_promote_from_reflection` (not loaded under core),
    // so the "more" count grows from 53 to 54.
    // v0.7.0 L2-7 (issue #672) — Family::Other gained
    // `memory_skill_compositional_context` (not loaded under core),
    // so the "more" count grows from 54 to 55.
    // v0.7.0 QW-1 — Family::Power gained `memory_export_reflection`
    // (not loaded under core), so the "more" count grows 55 → 56.
    // v0.7.0 QW-3 follow-up — Family::Power gained `memory_offload` +
    // `memory_deref` (not loaded under core), so the "more" count grows
    // from 56 to 58.
    // v0.7.0 WT-1-C — Family::Power gained `memory_atomise` (not
    // loaded under core), so the "more" count grows 58 → 59.
    // v0.7.0 QW-2 — Family::Power gained `memory_persona` +
    // `memory_persona_generate` (not loaded under core), so the
    // "more" count grows 59 → 61.
    // v0.7.0 Form 3 (#756) — Family::Power gained
    // `memory_ingest_multistep` (not loaded under core), so the "more"
    // count grows 61 → 62.
    // v0.7.0 Form 5 (#758) — Family::Power gained
    // `memory_calibrate_confidence` (not loaded under core), so the
    // "more" count grows 62 → 63.
    // v0.7.0 issues #224 + #311 — Family::Power gained `memory_share`
    // (not loaded under core), so the "more" count grows 63 → 64.
    let expected = "I can directly use 7 memory tools right now \
                    (store, recall, list, get, search, ...). 65 more \
                    (update, delete, forget, gc, etc.) are available on demand — \
                    I can load them if you ask for something that needs them, \
                    or you can restart the server with a different profile.";

    assert_eq!(
        describe, expected,
        "T0-A2-CORE: describe_to_user drifted from canonical phrasing.\n\
         expected: {expected}\n\
         actual:   {describe}"
    );
}

// ---------------------------------------------------------------------------
// T0-A2-FULL — `to_describe_to_user` on `--profile full` uses the
// "nothing more to load" closing form (excludes the always-on bootstrap
// from the user-facing count). Bumped from 42 to 43 in v0.7.0 I4 —
// Family::Graph gained `memory_replay`; to 44 in v0.7 H4 —
// Family::Graph gained `memory_verify`; to 45 in v0.7 B1 —
// Family::Core gained `memory_load_family`; to 47 in v0.7 K7 —
// Family::Power gained `memory_subscription_replay` +
// `memory_subscription_dlq_list`; to 48 in v0.7 J7 —
// Family::Graph gained `memory_find_paths`; to 49 in v0.7 B2 —
// Family::Core gained `memory_smart_load`; to 50 in v0.7 K8 —
// Family::Power gained `memory_quota_status`; to 51 in v0.7.0
// Task 4/8 (#655) — Family::Power gained `memory_reflect`; to 56
// in v0.7.0 L1-5 — Family::Other gained 5 memory_skill_* tools.
// ---------------------------------------------------------------------------
#[test]
fn t0_describe_to_user_full_profile_canonical_phrasing() {
    let val = v3_response(&Profile::full());
    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");

    // v0.7.0 L2-2 (S6-M1) — Family::Power gained
    // `memory_reflection_origin` → 52 visible (the "all 52" form
    // excludes the always-on `memory_capabilities` bootstrap from the
    // 53-tool total).
    // v0.7.0 (issue #691) — `memory_check_agent_action` +
    // `memory_rule_list` added to Family::Power → 54 visible (the
    // "all 54" form excludes the always-on `memory_capabilities`
    // bootstrap from the 55-tool total).
    // v0.7.0 L1-5 — 5 memory_skill_* tools added to Family::Other →
    // 59 visible (the "all 59" form excludes the always-on
    // `memory_capabilities` bootstrap from the 60-tool total).
    // v0.7.0 L2-3 (issue #668) — Family::Power gained
    // `memory_dependents_of_invalidated` → 60 visible (the "all 60"
    // form excludes the always-on `memory_capabilities` bootstrap
    // from the 61-tool total).
    // v0.7.0 L2-6 (issue #671) — Family::Other gained
    // `memory_skill_promote_from_reflection` → 61 visible.
    // v0.7.0 L2-7 (issue #672) — Family::Other gained
    // `memory_skill_compositional_context` → 62 visible.
    // v0.7.0 QW-1 — Family::Power gained `memory_export_reflection`
    // → 63 visible under full.
    // v0.7.0 QW-3 follow-up — Family::Power gained `memory_offload` +
    // `memory_deref` → 65 visible (out of the 66-tool total).
    // v0.7.0 WT-1-C — Family::Power gained `memory_atomise`
    // → 66 visible (out of the 67-tool total).
    // v0.7.0 QW-2 — Family::Power gained `memory_persona` +
    // `memory_persona_generate` → 68 visible under full (the "all 68"
    // form excludes the always-on `memory_capabilities` bootstrap from
    // the 69-tool total).
    // v0.7.0 Form 3 (#756) — Family::Power gained
    // `memory_ingest_multistep` → 69 visible under full (the "all 69"
    // form excludes the always-on `memory_capabilities` bootstrap from
    // the 70-tool total).
    // v0.7.0 Form 5 (#758) — Family::Power gained
    // `memory_calibrate_confidence` → 70 visible under full (the "all 70"
    // form excludes the always-on `memory_capabilities` bootstrap from
    // the 71-tool total).
    // v0.7.0 issues #224 + #311 — Family::Power gained `memory_share`
    // → 71 visible under full (the "all 71" form excludes the always-on
    // `memory_capabilities` bootstrap from the 72-tool total).
    let expected = "I can directly use all 72 memory tools right now \
                    (store, recall, list, get, search, ...). Nothing more to load — \
                    the full memory surface is already active.";

    assert_eq!(
        describe, expected,
        "T0-A2-FULL: describe_to_user drifted from canonical phrasing.\n\
         expected: {expected}\n\
         actual:   {describe}"
    );
}

// ---------------------------------------------------------------------------
// T0-A2-GRAPH — `to_describe_to_user` on `--profile graph` uses the
// preview-with-ellipsis form (5 of 18 loaded shown + ", ..."). Loaded
// bumped from 13 to 14 in v0.7.0 I4 — Family::Graph gained
// `memory_replay`; to 15 in v0.7 H4 — Family::Graph gained
// `memory_verify`; to 16 in v0.7 B1 — Family::Core gained
// `memory_load_family`; to 17 in v0.7 J7 — Family::Graph gained
// `memory_find_paths`; to 18 in v0.7 B2 — Family::Core gained
// `memory_smart_load`. Total bumped to 51 in v0.7 K8 — Family::Power
// gained `memory_quota_status` (not loaded under graph profile, so
// `more` count grew from 31 to 32). To 52 in v0.7.0 Task 4/8 (#655) —
// Family::Power gained `memory_reflect` (also not loaded under graph,
// so `more` count grows from 32 to 33). To 56 in v0.7.0 L1-5 —
// Family::Other gained 5 memory_skill_* tools (not loaded under graph,
// so `more` count grows from 33 to 38).
// ---------------------------------------------------------------------------
#[test]
fn t0_describe_to_user_graph_profile_canonical_phrasing() {
    let val = v3_response(&Profile::graph());
    let describe = val["to_describe_to_user"]
        .as_str()
        .expect("describe present");

    // v0.7.0 L2-2 (S6-M1) — Family::Power gained
    // `memory_reflection_origin` (not loaded under graph), so the
    // "more" count grows from 33 to 34.
    // v0.7.0 (issue #691) — `memory_check_agent_action` +
    // `memory_rule_list` added to Family::Power (not loaded under
    // graph), so the "more" count grows from 34 to 36.
    // v0.7.0 L1-5 — 5 memory_skill_* tools added to Family::Other (not
    // loaded under graph), so the "more" count grows from 36 to 41.
    // v0.7.0 L2-3 (issue #668) — Family::Power gained
    // `memory_dependents_of_invalidated` (not loaded under graph), so
    // the "more" count grows from 41 to 42.
    // v0.7.0 L2-6 (issue #671) — Family::Other gained
    // `memory_skill_promote_from_reflection` (not loaded under graph),
    // so the "more" count grows from 42 to 43.
    // v0.7.0 L2-7 (issue #672) — Family::Other gained
    // `memory_skill_compositional_context` (not loaded under graph),
    // so the "more" count grows from 43 to 44.
    // v0.7.0 QW-1 — Family::Power gained `memory_export_reflection`
    // (not loaded under graph), so the "more" count grows 44 → 45.
    // v0.7.0 QW-3 follow-up — Family::Power gained `memory_offload` +
    // `memory_deref` (not loaded under graph), so the "more" count grows
    // from 45 to 47.
    // v0.7.0 WT-1-C — Family::Power gained `memory_atomise`
    // (not loaded under graph), so the "more" count grows 47 → 48.
    // v0.7.0 QW-2 — Family::Power gained `memory_persona` +
    // `memory_persona_generate` (not loaded under graph), so the
    // "more" count grows 48 → 50.
    // v0.7.0 Form 3 (#756) — Family::Power gained
    // `memory_ingest_multistep` (not loaded under graph), so the
    // "more" count grows 50 → 51.
    // v0.7.0 Form 5 (#758) — Family::Power gained
    // `memory_calibrate_confidence` (not loaded under graph), so the
    // "more" count grows 51 → 52.
    // v0.7.0 issues #224 + #311 — Family::Power gained `memory_share`
    // (not loaded under graph), so the "more" count grows 52 → 53.
    let expected = "I can directly use 18 memory tools right now \
                    (store, recall, list, get, search, ...). 54 more \
                    (update, delete, forget, gc, etc.) are available on demand — \
                    I can load them if you ask for something that needs them, \
                    or you can restart the server with a different profile.";

    assert_eq!(
        describe, expected,
        "T0-A2-GRAPH: describe_to_user drifted from canonical phrasing.\n\
         expected: {expected}\n\
         actual:   {describe}"
    );
}

// ---------------------------------------------------------------------------
// T0-A2-NO-JARGON — `to_describe_to_user` MUST NOT contain MCP-internal
// vocabulary across ANY profile. This is the tone gate from
// docs/v0.7/canonical-phrasings.md §"Tone constraint".
// ---------------------------------------------------------------------------
#[test]
fn t0_describe_to_user_omits_mcp_jargon_across_profiles() {
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let val = v3_response(profile);
        let describe = val["to_describe_to_user"]
            .as_str()
            .expect("describe present");

        for forbidden in &[
            "--profile <family>",
            "--profile full",
            "memory_load_family",
            "memory_smart_load",
            "JSON-RPC",
            "-32601",
            "tools/list",
            "memory_",
        ] {
            assert!(
                !describe.contains(forbidden),
                "T0-A2-NO-JARGON: profile={profile:?}: describe_to_user contains MCP jargon \
                 \"{forbidden}\" — keep it plain for end users.\nfull: {describe}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// T0-A1-CORE — the `summary` (operator-facing) string on `--profile core`
// names the four recovery paths verbatim (a, b, c, d). This is the
// counterpart calibration cell for the A1 phrasing — operators get the
// recovery vocabulary even when LLMs mute it from the user-facing
// describe sentence.
// ---------------------------------------------------------------------------
#[test]
fn t0_summary_core_profile_lists_four_recovery_paths() {
    let val = v3_response(&Profile::core());
    let summary = val["summary"].as_str().expect("summary present");

    // Path (a) — CLI escape hatch
    assert!(
        summary.contains("(a) restart the server with --profile <family>"),
        "T0-A1-CORE: summary missing recovery path (a); got: {summary}"
    );
    // Path (b) — preferred runtime loader (B1, lands later in v0.7.0)
    assert!(
        summary.contains("(b) call memory_load_family(family=<name>) — preferred"),
        "T0-A1-CORE: summary missing recovery path (b); got: {summary}"
    );
    // Path (c) — easiest runtime loader (B2, lands later in v0.7.0)
    assert!(
        summary.contains("(c) call memory_smart_load(intent='<plain language>') — easiest"),
        "T0-A1-CORE: summary missing recovery path (c); got: {summary}"
    );
    // Path (d) — call-by-name fallback for harnesses without runtime loaders
    assert!(
        summary.contains("(d) call the tool by name and recover from JSON-RPC -32601"),
        "T0-A1-CORE: summary missing recovery path (d); got: {summary}"
    );
}

// ---------------------------------------------------------------------------
// T0-CONTRACT — both calibration strings are present and well-typed in
// every named profile's v3 response. Catches structural regressions
// (missing field, null instead of string, etc.) ahead of the per-string
// content tests above.
// ---------------------------------------------------------------------------
#[test]
fn t0_v3_contract_both_strings_present_under_every_named_profile() {
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let val = v3_response(profile);
        assert_eq!(
            val["schema_version"], "3",
            "T0-CONTRACT profile={profile:?}: schema_version missing or wrong"
        );
        assert!(
            val["summary"].as_str().is_some_and(|s| !s.is_empty()),
            "T0-CONTRACT profile={profile:?}: summary missing/empty"
        );
        assert!(
            val["to_describe_to_user"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "T0-CONTRACT profile={profile:?}: to_describe_to_user missing/empty"
        );
    }
}
