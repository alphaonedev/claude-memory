// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 follow-up — MCP-tool registration tests for the
//! context-offload substrate primitive.
//!
//! The QW-3 substrate (PR #741) merged with `handle_offload` /
//! `handle_deref` ready but NOT registered in
//! `tool_definitions_for_profile`. This follow-up wires both tools
//! into the `Family::Power` registration so `--profile power` (and
//! `--profile full`) surface them via `tools/list`, while the
//! keyword-tier `--profile core` minimum stays at its 7-tool surface.
//!
//! The substrate-level behaviour (round-trip, tampered-row refusal,
//! size limit) is pinned by `tests/offload/acceptance.rs`. This file
//! pins ONLY the registration cascade — Family mapping, per-profile
//! `loads(name)` truth table, and the pair-appears-together
//! invariant that future profile-count regressions would otherwise
//! mask.

use ai_memory::profile::{Family, Profile};

// ---------------------------------------------------------------------------
// 1. memory_offload registered at the semantic-tier+ Power family —
//    visible under `--profile power` and `--profile full` but NOT under
//    the keyword-tier `--profile core` minimum.
// ---------------------------------------------------------------------------
#[test]
fn test_memory_offload_registered_at_semantic_tier() {
    assert_eq!(
        Family::for_tool("memory_offload"),
        Some(Family::Power),
        "memory_offload must live in Family::Power (semantic-tier+ surface)"
    );

    // Power + full + admin-with-power custom profiles load it.
    assert!(
        Profile::power().loads("memory_offload"),
        "--profile power must surface memory_offload"
    );
    assert!(
        Profile::full().loads("memory_offload"),
        "--profile full must surface memory_offload"
    );
}

// ---------------------------------------------------------------------------
// 2. Keyword-tier `core` profile does NOT surface memory_offload — the
//    7-tool keyword-tier minimum stays at its original surface so an
//    eager-loading harness that picks `--profile core` to minimise
//    schema tokens still pays the same prefix cost it paid pre-QW-3.
// ---------------------------------------------------------------------------
#[test]
fn test_memory_offload_absent_at_keyword_tier() {
    let core = Profile::core();
    assert!(
        !core.loads("memory_offload"),
        "--profile core must NOT surface memory_offload (QW-3 brief: \
         semantic-tier+ exposure only)"
    );
    assert!(
        !core.loads("memory_deref"),
        "--profile core must NOT surface memory_deref (paired with offload)"
    );

    // Graph / admin profiles (also keyword-tier-style minimums without
    // the power family) likewise omit the pair.
    let graph = Profile::graph();
    assert!(!graph.loads("memory_offload"));
    assert!(!graph.loads("memory_deref"));
    let admin = Profile::admin();
    assert!(!admin.loads("memory_offload"));
    assert!(!admin.loads("memory_deref"));
}

// ---------------------------------------------------------------------------
// 3. The offload + deref pair is registered together — never one without
//    the other. A regression that registered just one half would leave
//    callers with a substrate primitive they cannot round-trip
//    (`offload` returns a `ref_id` whose only consumer is `deref`, and
//    vice versa).
// ---------------------------------------------------------------------------
#[test]
fn test_memory_deref_pair_registered() {
    // Both tools resolve to the same family.
    assert_eq!(Family::for_tool("memory_offload"), Some(Family::Power));
    assert_eq!(Family::for_tool("memory_deref"), Some(Family::Power));

    // Across every profile that loads one, the other loads too.
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let offload = profile.loads("memory_offload");
        let deref = profile.loads("memory_deref");
        assert_eq!(
            offload, deref,
            "profile={profile:?}: memory_offload and memory_deref must be \
             registered together (loads(offload)={offload}, loads(deref)={deref})"
        );
    }

    // Both must appear in the Power family's canonical tool_names list
    // so the family map + family preview surfaces stay aligned.
    let power_names = Family::Power.tool_names();
    assert!(
        power_names.contains(&"memory_offload"),
        "Family::Power.tool_names() must contain memory_offload; got: {power_names:?}"
    );
    assert!(
        power_names.contains(&"memory_deref"),
        "Family::Power.tool_names() must contain memory_deref; got: {power_names:?}"
    );
}

// ---------------------------------------------------------------------------
// 4. (Bonus, complements the three required tests above.) The pair's
//    expected_tool_count contribution to Family::Power matches the
//    advertised family size. Catches a half-baked cherry-pick that adds
//    the names to `for_tool` / `tool_names` but forgets to bump the
//    `expected_tool_count` counter (or vice versa) — exactly the
//    failure mode the family_tool_names_match_expected_count unit test
//    in src/profile.rs pins per-family. Mirrored here at the
//    integration boundary so a future profile-count audit can run
//    against the integration crate alone.
// ---------------------------------------------------------------------------
#[test]
fn test_offload_pair_in_family_power_expected_count() {
    let power_names = Family::Power.tool_names();
    assert_eq!(
        power_names.len(),
        Family::Power.expected_tool_count(),
        "Family::Power tool_names length must match expected_tool_count; \
         got names.len()={n}, expected_tool_count()={c}",
        n = power_names.len(),
        c = Family::Power.expected_tool_count()
    );
}
