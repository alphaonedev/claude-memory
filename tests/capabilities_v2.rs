// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Integration tests for the **Capabilities v2 honesty schema** (P1 of
//! the v0.6.3.1 remediation, REMEDIATIONv0631.md §"Phase P1").
//!
//! The honesty patch:
//! - Replaces `features.memory_reflection: bool` with a
//!   `{planned, version, enabled}` object.
//! - Adds `features.recall_mode_active` (live runtime tag) and
//!   `features.reranker_active` (derived from the actual `CrossEncoder`
//!   variant).
//! - Drops fields that have no backing implementation:
//!   `permissions.rule_summary`, `hooks.by_event`,
//!   `approval.subscribers`, `approval.default_timeout_seconds`.
//! - Marks planned features explicitly: `compaction`, `transcripts`,
//!   `memory_reflection`.
//! - Renames `permissions.mode` from `"ask"` (implied an interactive
//!   prompt loop) to `"advisory"`.
//! - Preserves backward compat via `Accept-Capabilities: v1` (HTTP) or
//!   the MCP `accept` argument set to `"v1"`.
//!
//! These tests pin the honest contract so future drift surfaces in CI.

use ai_memory::config::{
    Capabilities, CapabilitiesV1, CapabilityFeatures, FeatureTier, RecallMode, RerankerMode,
    TierConfig,
};
use ai_memory::mcp::{CapabilitiesAccept, handle_capabilities_with_conn};
use ai_memory::reranker::CrossEncoder;
use serde_json::Value;

/// Build a fresh in-memory `rusqlite::Connection` so each test gets a
/// clean DB state for the live-count overlays.
fn fresh_conn() -> rusqlite::Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

// ---------------------------------------------------------------------------
// Cap-v2 reports recall_mode_active = "keyword_only" (now: disabled) when
// the daemon is on the keyword tier with no embedder configured.
//
// Spec note: the REMEDIATIONv0631 audit calls for `keyword_only` when "no
// embedder", but the honesty patch refines the semantics — `Disabled` is
// returned for the keyword tier (semantic recall is not configured at all);
// `KeywordOnly` is reserved for a future operator-disabled-blending state.
// `Degraded` covers the "configured but failed to load" case. The audit's
// intent (no semantic recall ⇒ truthful tag) is preserved.
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_reports_recall_mode_keyword_only_when_no_embedder() {
    let tier_config = FeatureTier::Keyword.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,  // no reranker
        false, // no embedder loaded
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    assert_eq!(
        val["features"]["recall_mode_active"], "disabled",
        "keyword tier with no embedder must report recall_mode_active=disabled"
    );
    assert_eq!(val["features"]["semantic_search"], false);
    assert_eq!(val["features"]["embedder_loaded"], false);
}

// ---------------------------------------------------------------------------
// Cap-v2 reports reranker_active = "off" when the reranker is disabled at
// startup (no `CrossEncoder` handle in the daemon at all).
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_reports_reranker_off_when_disabled_at_startup() {
    let tier_config = FeatureTier::Semantic.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None, // no reranker handle = "off"
        false,
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    assert_eq!(
        val["features"]["reranker_active"], "off",
        "no reranker handle must report reranker_active=off"
    );
    assert_eq!(val["features"]["cross_encoder_reranking"], false);
    assert_eq!(val["models"]["cross_encoder"], "none");
}

// ---------------------------------------------------------------------------
// Cap-v2 reports reranker_active = "lexical_fallback" when the neural
// cross-encoder failed to load and the daemon dropped to the lexical scorer.
// `CrossEncoder::Lexical` is exactly that signal — it's what
// `CrossEncoder::new_neural()` returns when the HF download fails (see
// `src/reranker.rs` `load_neural`).
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_reports_reranker_lexical_fallback_when_neural_init_failed() {
    let tier_config = FeatureTier::Autonomous.config();
    let lexical = CrossEncoder::new(); // Lexical variant — same as a failed neural load
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        Some(&lexical),
        true,
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    assert_eq!(
        val["features"]["reranker_active"], "lexical_fallback",
        "lexical CrossEncoder variant must report reranker_active=lexical_fallback"
    );
    // Honesty fix #93 (predates P1): cross_encoder_reranking flips false
    // when the neural model failed to load.
    assert_eq!(val["features"]["cross_encoder_reranking"], false);
    assert!(
        val["models"]["cross_encoder"]
            .as_str()
            .unwrap()
            .contains("lexical-fallback"),
        "cross_encoder model name must annotate the fallback"
    );
}

// ---------------------------------------------------------------------------
// Cap-v2 omits the dropped fields: `permissions.rule_summary`,
// `hooks.by_event`, `approval.subscribers`,
// `approval.default_timeout_seconds`. Each had no backing implementation;
// reporting them implied features that did not exist.
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_omits_dropped_fields_in_v2_response() {
    let tier_config = FeatureTier::Smart.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        true,
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    // schema_version discriminator
    assert_eq!(val["schema_version"], "2");

    // Dropped from v2 — must NOT appear in the JSON.
    assert!(
        val["permissions"].get("rule_summary").is_none(),
        "permissions.rule_summary must be absent from v2 (no per-rule serializer existed)"
    );
    assert!(
        val["hooks"].get("by_event").is_none(),
        "hooks.by_event must be absent from v2 (no event registry existed)"
    );
    assert!(
        val["approval"].get("subscribers").is_none(),
        "approval.subscribers must be absent from v2 (no subscription API existed)"
    );
    assert!(
        val["approval"].get("default_timeout_seconds").is_none(),
        "approval.default_timeout_seconds must be absent from v2 (no sweeper enforced timeouts)"
    );

    // permissions.mode was renamed from "ask" to "advisory" — the old
    // value implied an interactive prompt loop the code does not have.
    assert_eq!(
        val["permissions"]["mode"], "advisory",
        "permissions.mode must be 'advisory' until P4 ships the enforcement gate"
    );

    // Planned-feature objects (memory_reflection, compaction, transcripts).
    assert_eq!(val["features"]["memory_reflection"]["planned"], true);
    assert_eq!(val["features"]["memory_reflection"]["enabled"], false);
    assert_eq!(val["features"]["memory_reflection"]["version"], "v0.7+");
    assert_eq!(val["compaction"]["planned"], true);
    assert_eq!(val["compaction"]["enabled"], false);
    assert_eq!(val["compaction"]["version"], "v0.8+");
    assert_eq!(val["transcripts"]["planned"], true);
    assert_eq!(val["transcripts"]["enabled"], false);
    assert_eq!(val["transcripts"]["version"], "v0.7+");
}

// ---------------------------------------------------------------------------
// v1 backward compat: when client sends `accept = "v1"` the daemon returns
// the legacy shape — no `schema_version`, no v2-only blocks,
// `memory_reflection` is a bool. Pre-v0.6.3.1 callers that pinned the v1
// schema continue to pass.
// ---------------------------------------------------------------------------
#[test]
fn cap_v1_compat_returns_legacy_shape_on_accept_header() {
    let tier_config = FeatureTier::Autonomous.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        true,
        Some(&conn),
        CapabilitiesAccept::V1,
    )
    .expect("v1 capabilities serialize");

    // v1 wire shape: no schema_version, no v2-only blocks.
    assert!(
        val.get("schema_version").is_none(),
        "v1 has no schema_version field"
    );
    assert!(
        val.get("permissions").is_none(),
        "v1 has no permissions block"
    );
    assert!(val.get("hooks").is_none(), "v1 has no hooks block");
    assert!(
        val.get("compaction").is_none(),
        "v1 has no compaction block"
    );
    assert!(val.get("approval").is_none(), "v1 has no approval block");
    assert!(
        val.get("transcripts").is_none(),
        "v1 has no transcripts block"
    );

    // v1 keeps the four legacy top-level keys.
    assert!(val["tier"].is_string());
    assert!(val["version"].is_string());
    assert!(val["features"].is_object());
    assert!(val["models"].is_object());

    // memory_reflection collapses to a bool in v1.
    assert!(
        val["features"]["memory_reflection"].is_boolean(),
        "v1 features.memory_reflection is a bool, not the v2 object"
    );

    // v1 features carries no recall_mode_active / reranker_active —
    // those are v2-only honesty fields.
    assert!(val["features"].get("recall_mode_active").is_none());
    assert!(val["features"].get("reranker_active").is_none());

    // v1 deserializes back to the typed CapabilitiesV1.
    let restored: CapabilitiesV1 =
        serde_json::from_value(val).expect("v1 round-trip through CapabilitiesV1");
    assert_eq!(restored.tier, "autonomous");
}

// ---------------------------------------------------------------------------
// Bonus: recall_mode_active flips to "hybrid" when the embedder is loaded
// on a tier that configured it. Pins the live-overlay logic.
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_recall_mode_hybrid_when_embedder_loaded_on_semantic_tier() {
    let tier_config = FeatureTier::Semantic.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        true, // embedder loaded
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    assert_eq!(
        val["features"]["recall_mode_active"], "hybrid",
        "embedder loaded on semantic tier ⇒ recall_mode_active=hybrid"
    );
    assert_eq!(val["features"]["embedder_loaded"], true);
}

// ---------------------------------------------------------------------------
// Bonus: recall_mode_active = "degraded" when the embedder was configured
// (semantic / smart / autonomous tier) but failed to materialize at startup.
// Operators reading capabilities can then refuse to dispatch semantic-recall
// scenarios against a daemon that thinks it's semantic but isn't.
// ---------------------------------------------------------------------------
#[test]
fn cap_v2_recall_mode_degraded_when_embedder_configured_but_not_loaded() {
    let tier_config = FeatureTier::Smart.config();
    let conn = fresh_conn();
    let val = handle_capabilities_with_conn(
        &tier_config,
        None,
        false, // configured but not loaded — HF download failed, etc.
        Some(&conn),
        CapabilitiesAccept::V2,
    )
    .expect("v2 capabilities serialize");

    assert_eq!(
        val["features"]["recall_mode_active"], "degraded",
        "embedder configured but not loaded ⇒ recall_mode_active=degraded"
    );
    assert_eq!(val["features"]["embedder_loaded"], false);
}

// ---------------------------------------------------------------------------
// Accept-string parsing: case + whitespace tolerant. Explicit `"v1"`/`"v2"`
// still resolve to V1/V2; v0.7.0 A5 flips the unknown/missing default from
// V2 to V3, so any non-explicit value now resolves to V3 instead.
// ---------------------------------------------------------------------------
#[test]
fn cap_accept_parse_is_case_insensitive_and_defaults_to_v3() {
    assert_eq!(CapabilitiesAccept::parse("v1"), CapabilitiesAccept::V1);
    assert_eq!(CapabilitiesAccept::parse(" V1 "), CapabilitiesAccept::V1);
    assert_eq!(CapabilitiesAccept::parse("1"), CapabilitiesAccept::V1);
    assert_eq!(CapabilitiesAccept::parse("v2"), CapabilitiesAccept::V2);
    assert_eq!(CapabilitiesAccept::parse("V2"), CapabilitiesAccept::V2);
    assert_eq!(CapabilitiesAccept::parse("2"), CapabilitiesAccept::V2);
    // v0.7.0 A5: unknown / empty falls back to v3 (was v2 pre-A5).
    assert_eq!(CapabilitiesAccept::parse(""), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse("v9"), CapabilitiesAccept::V3);
    assert_eq!(CapabilitiesAccept::parse("garbage"), CapabilitiesAccept::V3);
}

// ---------------------------------------------------------------------------
// Static type-system probe: confirm the `CapabilityFeatures.recall_mode_active`
// and `reranker_active` field types are the new enums (not strings or bools).
// Compile-time check; if the schema regresses to a primitive, this fails to
// compile and the regression surfaces immediately.
// ---------------------------------------------------------------------------
#[test]
fn cap_struct_field_types_are_typed_enums_not_primitives() {
    let cfg: TierConfig = FeatureTier::Autonomous.config();
    let caps: Capabilities = cfg.capabilities();
    // The next three lines only compile if the struct fields hold the
    // honest types (`&CapabilityFeatures`, `RecallMode`, `RerankerMode`).
    // A regression to a primitive bool/string or to a different struct
    // would fail to coerce.
    let features: &CapabilityFeatures = &caps.features;
    let recall: RecallMode = features.recall_mode_active;
    let reranker: RerankerMode = features.reranker_active;
    assert_eq!(recall, RecallMode::Disabled);
    assert_eq!(reranker, RerankerMode::Off);
    // A JSON probe gives us a runtime assert as well — the variants
    // serialize to snake_case strings, never to bools or numbers.
    let v: Value = serde_json::to_value(&caps).unwrap();
    assert!(v["features"]["recall_mode_active"].is_string());
    assert!(v["features"]["reranker_active"].is_string());
}
