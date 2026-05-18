// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Discovery Gate **T1 / T2 / T3 loader cells** — extend the T0 calibration
//! (`tests/calibration_t0.rs`) with the three deeper observation tiers
//! defined by the NHI Discovery Gate spec, focused on the v0.7.0 recovery
//! vocabulary: `--profile`, `memory_load_family` (B1) and
//! `memory_smart_load` (B2).
//!
//! T0 (in `calibration_t0.rs`) pins the canonical phrasings byte-for-byte.
//! E3 layers the recall / recognition / use tiers on top of that
//! substrate so the CI surface mirrors what the LLM observation cells
//! ask for in the wild:
//!
//! - **T1 RECALL** — given the v3 capabilities response, can a
//!   reasoning-class LLM recall the **names** of the three runtime
//!   recovery paths (`--profile`, `memory_load_family`,
//!   `memory_smart_load`)? Asserted as substring presence in `summary`.
//! - **T2 RECOGNITION** — given a user-style question ("how do I load
//!   more tools?"), does the substrate's `to_describe_to_user` string
//!   carry the recognition lexicon ("on demand", "load them",
//!   "different profile") that an LLM is expected to repeat verbatim?
//! - **T3 USE** — simulate the LLM actually invoking
//!   `memory_load_family(family=...)` or
//!   `memory_smart_load(intent=...)`. These are **`#[ignore]`d** today
//!   because B1 + B2 have not shipped yet — the cells light up
//!   automatically once those tools land in the always-on registry.
//!
//! Discovery Gate verdict mapping (see `docs/v0.7/v0.7-nhi-prompts.md`
//! § E3):
//! - `t1-awareness-loaders.md` → `t1_recall_*` cells in this file
//! - `t2-reactive-loaders.md`  → `t2_recognition_*` cells in this file
//! - `t3-proactive-smart-load.md` → `t3_use_*` cells (ignored until
//!   B1+B2 ship)
//!
//! When B1 and B2 land, drop the `#[ignore]` attributes from the
//! `t3_use_*` cells and wire them through the real tool dispatcher.
//! Until then the T3 cells stand as live documentation of the expected
//! call shape.

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

fn summary_for(profile: &Profile) -> String {
    v3_response(profile)["summary"]
        .as_str()
        .expect("summary present")
        .to_string()
}

fn describe_for(profile: &Profile) -> String {
    v3_response(profile)["to_describe_to_user"]
        .as_str()
        .expect("describe present")
        .to_string()
}

// ===========================================================================
// T1 — RECALL cells.
//
// The LLM has just read `memory_capabilities` and must recall the names
// of the three runtime recovery paths. The substrate side of that
// contract is that all three names appear verbatim in the operator-facing
// `summary` string under every named profile (so the LLM's training
// surface always sees them, regardless of how the daemon was started).
// ===========================================================================

// ---------------------------------------------------------------------------
// T1-RECALL-PROFILE — `--profile <family>` is recallable from the
// summary under every named profile (path (a) in the canonical
// phrasing).
// ---------------------------------------------------------------------------
#[test]
fn t1_recall_profile_flag_named_in_summary_under_every_profile() {
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let summary = summary_for(profile);
        assert!(
            summary.contains("--profile"),
            "T1-RECALL-PROFILE: profile={profile:?} — `--profile` must be recallable \
             from `summary` (recovery path (a)).\nfull: {summary}"
        );
    }
}

// ---------------------------------------------------------------------------
// T1-RECALL-LOAD-FAMILY — `memory_load_family` is recallable from the
// summary under every named profile (path (b) in the canonical
// phrasing). B1 has not shipped, but the *vocabulary* is part of the
// teach-the-LLM surface today.
// ---------------------------------------------------------------------------
#[test]
fn t1_recall_memory_load_family_named_in_summary_under_every_profile() {
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let summary = summary_for(profile);
        assert!(
            summary.contains("memory_load_family"),
            "T1-RECALL-LOAD-FAMILY: profile={profile:?} — `memory_load_family` must \
             be recallable from `summary` (recovery path (b)).\nfull: {summary}"
        );
    }
}

// ---------------------------------------------------------------------------
// T1-RECALL-SMART-LOAD — `memory_smart_load` is recallable from the
// summary under every named profile (path (c) in the canonical
// phrasing). Same shipping note as the load-family cell — vocabulary
// taught now, callable when B2 lands.
// ---------------------------------------------------------------------------
#[test]
fn t1_recall_memory_smart_load_named_in_summary_under_every_profile() {
    for profile in &[
        Profile::core(),
        Profile::graph(),
        Profile::admin(),
        Profile::power(),
        Profile::full(),
    ] {
        let summary = summary_for(profile);
        assert!(
            summary.contains("memory_smart_load"),
            "T1-RECALL-SMART-LOAD: profile={profile:?} — `memory_smart_load` must \
             be recallable from `summary` (recovery path (c)).\nfull: {summary}"
        );
    }
}

// ---------------------------------------------------------------------------
// T1-RECALL-ALL-THREE-DISTINCT — the three loader-recovery names are
// not collapsed into one another. A regression that reduced (b)+(c) to
// just (b) (or vice versa) would still pass the per-name cells above
// because the surviving name is a substring; this cell catches that
// drift by asserting both distinct names appear in the same response.
// ---------------------------------------------------------------------------
#[test]
fn t1_recall_loader_names_remain_distinct_in_summary() {
    let summary = summary_for(&Profile::core());

    let load_family_idx = summary
        .find("memory_load_family")
        .expect("memory_load_family must be present");
    let smart_load_idx = summary
        .find("memory_smart_load")
        .expect("memory_smart_load must be present");

    assert_ne!(
        load_family_idx, smart_load_idx,
        "T1-RECALL-ALL-THREE-DISTINCT: the two loader names collapsed to a \
         single occurrence — the canonical phrasing requires both as separate \
         recovery paths.\nfull: {summary}"
    );
}

// ===========================================================================
// T2 — RECOGNITION cells.
//
// The LLM is asked an end-user-style question ("how do I load more
// tools?", "what tools do you have?"). The recognition test is that the
// substrate's `to_describe_to_user` carries the *recognition lexicon*
// the LLM is expected to converge on — the plain-English phrases that
// signal "more available, loadable on demand" without leaking MCP
// jargon. These mirror the T2 observation cell rubrics in
// `docs/v0.7/v0.7-nhi-prompts.md` § E3.
// ===========================================================================

// ---------------------------------------------------------------------------
// T2-RECOGNITION-AVAILABLE-ON-DEMAND — partial profiles signal that
// more tools exist and are loadable on demand. This is the lexicon the
// LLM should reach for when a user asks "is that all you can do?".
// ---------------------------------------------------------------------------
#[test]
fn t2_recognition_describe_signals_load_on_demand_under_partial_profile() {
    for profile in &[Profile::core(), Profile::graph(), Profile::admin()] {
        let describe = describe_for(profile);
        assert!(
            describe.contains("on demand"),
            "T2-RECOGNITION-AVAILABLE-ON-DEMAND: profile={profile:?} — describe \
             missing the \"on demand\" recognition phrase.\nfull: {describe}"
        );
        assert!(
            describe.contains("load them"),
            "T2-RECOGNITION-AVAILABLE-ON-DEMAND: profile={profile:?} — describe \
             missing the \"load them\" recognition phrase.\nfull: {describe}"
        );
    }
}

// ---------------------------------------------------------------------------
// T2-RECOGNITION-DIFFERENT-PROFILE — partial-profile describe text
// names the operator-side escape hatch ("restart the server with a
// different profile") in plain English, without leaking the
// `--profile` CLI flag (that lives in `summary`, not `to_describe_to_user`).
// ---------------------------------------------------------------------------
#[test]
fn t2_recognition_describe_names_profile_escape_hatch_in_plain_english() {
    let describe = describe_for(&Profile::core());
    assert!(
        describe.contains("different profile"),
        "T2-RECOGNITION-DIFFERENT-PROFILE: core describe missing the \
         \"different profile\" plain-English escape-hatch reference.\nfull: {describe}"
    );
    assert!(
        describe.contains("restart the server"),
        "T2-RECOGNITION-DIFFERENT-PROFILE: core describe missing the \
         \"restart the server\" recognition phrase.\nfull: {describe}"
    );
}

// ---------------------------------------------------------------------------
// T2-RECOGNITION-FULL-CLOSING — the `--profile full` describe answers
// the same question with the closing form: nothing more to load. The
// recognition lexicon flips: the LLM should NOT reach for any loader
// vocabulary when the surface is already complete.
// ---------------------------------------------------------------------------
#[test]
fn t2_recognition_describe_uses_closing_form_under_full_profile() {
    let describe = describe_for(&Profile::full());
    assert!(
        describe.contains("Nothing more to load"),
        "T2-RECOGNITION-FULL-CLOSING: full describe missing the \
         \"Nothing more to load\" closing form.\nfull: {describe}"
    );
    assert!(
        !describe.contains("on demand"),
        "T2-RECOGNITION-FULL-CLOSING: full describe must NOT carry the \
         \"on demand\" loader lexicon — there is nothing left to load.\nfull: {describe}"
    );
}

// ---------------------------------------------------------------------------
// T2-RECOGNITION-OPERATOR-VOCAB — the `summary` (operator-facing) string
// carries the "preferred" / "easiest" recognition tags that let the LLM
// rank the loader paths without re-deriving them. This is the lexicon
// the t2-reactive-loaders cell expects the LLM to repeat when asked
// "what's the best way to load a family?".
// ---------------------------------------------------------------------------
#[test]
fn t2_recognition_summary_ranks_loader_paths_with_preferred_and_easiest() {
    let summary = summary_for(&Profile::core());
    assert!(
        summary.contains("— preferred"),
        "T2-RECOGNITION-OPERATOR-VOCAB: summary missing the \"— preferred\" \
         tag on memory_load_family (path (b)).\nfull: {summary}"
    );
    assert!(
        summary.contains("— easiest"),
        "T2-RECOGNITION-OPERATOR-VOCAB: summary missing the \"— easiest\" \
         tag on memory_smart_load (path (c)).\nfull: {summary}"
    );
}

// ===========================================================================
// T3 — USE cells.
//
// The LLM, having recalled (T1) and recognized (T2) the loader
// vocabulary, now invokes the loader. Today both `memory_load_family`
// (B1) and `memory_smart_load` (B2) are *named* in the canonical
// phrasing but are NOT yet wired through the always-on tool registry.
// The T3 cells below encode the expected call shape so they light up
// the moment the substrate ships.
//
// HOW TO RE-ENABLE:
//   1. B1/B2 land in `src/mcp.rs` always-on registry.
//   2. Remove the `#[ignore]` attribute from each cell below.
//   3. Replace the TODO with a real
//      `handle_tools_call_with_conn(...)` invocation against
//      `memory_load_family` / `memory_smart_load`.
//   4. CI now verifies the loaders WORK, not just that they're named.
// ===========================================================================

// ---------------------------------------------------------------------------
// T3-USE-LOAD-FAMILY — once B1 lands, asserting that
// memory_load_family(family="graph") returns success and that the
// resulting capabilities response now lists graph-family tools as
// loaded is the proof that the recall+recognition surface translated
// into a working call. Lights up when B1 ships.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "lights up when B1 (memory_load_family) lands in the always-on registry"]
fn t3_use_memory_load_family_loads_graph_family_at_runtime() {
    // Until B1 ships there is nothing meaningful to call. Document the
    // intended shape so the moment the tool exists this cell becomes
    // a one-line edit:
    //
    //   let resp = handle_tools_call_with_conn(
    //       "memory_load_family",
    //       json!({"family": "graph"}),
    //       Some(&conn),
    //       &mut profile,
    //       ...,
    //   ).expect("memory_load_family call");
    //   assert_eq!(resp["status"], "loaded");
    //   let after = v3_response(&profile);
    //   assert!(after["families"]["graph"]["loaded"].as_bool() == Some(true));
    panic!("T3-USE-LOAD-FAMILY: B1 not shipped yet — un-ignore when memory_load_family lands");
}

// ---------------------------------------------------------------------------
// T3-USE-SMART-LOAD-INTENT — once B2 lands, the same recall surface
// should drive an intent-routed load
// (`memory_smart_load(intent="store something")` → core family). This
// is the t3-proactive-smart-load.md observation cell's pass condition
// translated into a substrate-side assertion.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "lights up when B2 (memory_smart_load) lands in the always-on registry"]
fn t3_use_memory_smart_load_intent_routes_store_request_to_core_family() {
    // Intended shape (un-ignore + wire through the dispatcher when B2
    // ships):
    //
    //   let resp = handle_tools_call_with_conn(
    //       "memory_smart_load",
    //       json!({"intent": "store something"}),
    //       Some(&conn),
    //       &mut profile,
    //       ...,
    //   ).expect("memory_smart_load call");
    //   assert_eq!(resp["resolved_family"], "core",
    //       "intent='store something' must route to the core family");
    //   assert_eq!(resp["status"], "loaded");
    panic!("T3-USE-SMART-LOAD-INTENT: B2 not shipped yet — un-ignore when memory_smart_load lands");
}

// ---------------------------------------------------------------------------
// T3-USE-IDEMPOTENT-LOAD — calling memory_load_family twice for the
// same family is a no-op the second time (the loader is idempotent so
// LLMs that double-fire on retry don't observe state churn). Lights up
// alongside B1.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "lights up when B1 (memory_load_family) lands in the always-on registry"]
fn t3_use_memory_load_family_is_idempotent_under_repeated_calls() {
    // Intended shape:
    //
    //   let first = handle_tools_call_with_conn("memory_load_family",
    //       json!({"family": "graph"}), ...)?;
    //   let second = handle_tools_call_with_conn("memory_load_family",
    //       json!({"family": "graph"}), ...)?;
    //   assert_eq!(first["status"], "loaded");
    //   assert_eq!(second["status"], "already_loaded",
    //       "second call must be a no-op so retry-on-flake is safe");
    panic!("T3-USE-IDEMPOTENT-LOAD: B1 not shipped yet — un-ignore when memory_load_family lands");
}

// ---------------------------------------------------------------------------
// T3-USE-SMART-LOAD-AMBIGUOUS-INTENT — `memory_smart_load` must reject
// or disambiguate intents it can't route, rather than silently picking
// a default family. This guards against the LLM-side anti-pattern of
// passing through user free-text without checking the resolution.
// Lights up alongside B2.
// ---------------------------------------------------------------------------
#[test]
#[ignore = "lights up when B2 (memory_smart_load) lands in the always-on registry"]
fn t3_use_memory_smart_load_rejects_ambiguous_intent_without_silent_default() {
    // Intended shape:
    //
    //   let resp = handle_tools_call_with_conn("memory_smart_load",
    //       json!({"intent": "do the thing"}), ...);
    //   match resp {
    //       Err(e) => assert!(e.to_string().contains("ambiguous"),
    //           "ambiguous intent must surface — got: {e}"),
    //       Ok(v) => assert!(v["candidates"].as_array().is_some_and(|a| a.len() > 1),
    //           "ambiguous intent must return candidate families, not silent pick"),
    //   }
    panic!(
        "T3-USE-SMART-LOAD-AMBIGUOUS-INTENT: B2 not shipped yet — un-ignore when \
         memory_smart_load lands"
    );
}
