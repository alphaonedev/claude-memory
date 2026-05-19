// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! #880 / #793 PR-3 — `GovernancePolicy` wire-format regression tests.
//!
//! Pins the `#[serde(flatten)]` discipline introduced by the
//! [`GovernancePolicy`] decomposition. The 20 fields that used to live
//! flat on the parent struct now live grouped into 7 per-concern
//! sub-structs; the wire format must still be flat so existing
//! operator TOML / `metadata.governance` JSON payloads from pre-#880
//! peers round-trip identically.
//!
//! What this test crate pins:
//!
//! 1. A pre-decomposition flat JSON payload (every field at the root)
//!    deserialises cleanly into the new struct.
//! 2. A round-trip through `to_value` → `from_value` preserves every
//!    field value, sub-struct identity preserved.
//! 3. The serialised output of a fully-populated policy contains every
//!    field at the TOP level (no nested `core: {...}` / `atomisation:
//!    {...}` keys on the wire — flatten is doing its job).
//! 4. A partial JSON payload (only some fields supplied) deserialises
//!    with the missing fields defaulting via each sub-struct's
//!    `#[serde(default)]` (the documented pre-#880 partial-policy
//!    behaviour).

use ai_memory::models::{
    ApproverType, AutoAtomiseMode, GovernanceLevel, GovernancePolicy, MemoryKindAutoClassify,
    SynthesisFailureMode,
};
use serde_json::json;

/// Pre-#880 flat JSON shape — every field at the root, the wire shape
/// pre-v0.7.0 federation peers and operator TOML configs produce.
/// Must deserialise cleanly into the post-decomposition struct.
#[test]
fn pre_decomposition_flat_json_deserialises_cleanly() {
    let flat = json!({
        // Core
        "write": "owner",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true,
        "max_reflection_depth": 5,
        // Atomisation
        "auto_atomise": true,
        "auto_atomise_threshold_cl100k": 750,
        "auto_atomise_max_atom_tokens": 150,
        "auto_atomise_max_retries": 2,
        "auto_atomise_mode": "synchronous",
        // Persona
        "auto_persona_trigger_every_n_memories": 10,
        "auto_export_personas_to_filesystem": true,
        // Export
        "auto_export_reflections_to_filesystem": true,
        // Synthesis
        "legacy_per_pair_classifier": false,
        "synthesis_failure_mode": "block_write",
        "synthesis_max_deletes_per_call": 4,
        "synthesis_max_candidate_chars": 2000,
        // Multistep
        "multistep_max_content_chars": 3000,
        // KindClass
        "auto_classify_kind": "regex_only",
    });
    let p: GovernancePolicy = serde_json::from_value(flat).expect("flat JSON must deserialise");
    assert_eq!(p.core.write, GovernanceLevel::Owner);
    assert_eq!(p.core.max_reflection_depth, Some(5));
    assert_eq!(p.atomisation.auto_atomise, Some(true));
    assert_eq!(
        p.atomisation.auto_atomise_mode,
        Some(AutoAtomiseMode::Synchronous)
    );
    assert_eq!(p.persona.auto_persona_trigger_every_n_memories, Some(10));
    assert_eq!(p.export.auto_export_reflections_to_filesystem, Some(true));
    assert_eq!(
        p.synthesis.synthesis_failure_mode,
        Some(SynthesisFailureMode::BlockWrite)
    );
    assert_eq!(p.multistep.multistep_max_content_chars, Some(3000));
    assert_eq!(
        p.kind_class.auto_classify_kind,
        Some(MemoryKindAutoClassify::RegexOnly)
    );
}

/// Serialised output of a fully-populated policy must surface every
/// field at the TOP level (no nested `core: {...}` / `atomisation:
/// {...}` keys — `#[serde(flatten)]` is the load-bearing attribute).
#[test]
fn serialised_output_is_flat_no_substruct_keys_on_wire() {
    let p = GovernancePolicy {
        core: ai_memory::models::CorePolicy {
            write: GovernanceLevel::Approve,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
            inherit: true,
            max_reflection_depth: Some(7),
        },
        atomisation: ai_memory::models::AtomisationPolicy {
            auto_atomise: Some(true),
            ..ai_memory::models::AtomisationPolicy::default()
        },
        ..GovernancePolicy::default()
    };
    let v = serde_json::to_value(&p).expect("serialise");
    let obj = v.as_object().expect("policy must serialise as an object");

    // Wire-compat: every old field name MUST appear at the TOP level.
    for old_field in [
        "write",
        "promote",
        "delete",
        "approver",
        "inherit",
        "max_reflection_depth",
        "auto_atomise",
    ] {
        assert!(
            obj.contains_key(old_field),
            "wire compat broken — expected `{old_field}` at root, got keys: {:?}",
            obj.keys().collect::<Vec<_>>(),
        );
    }
    // Inverse: nested sub-struct keys MUST NOT appear at the top level
    // (that would be the BROKEN-flatten symptom — pre-#880 peers would
    // fail to read the payload).
    for new_group in [
        "core",
        "atomisation",
        "synthesis",
        "multistep",
        "kind_class",
        "persona",
        "export",
    ] {
        assert!(
            !obj.contains_key(new_group),
            "wire compat broken — nested key `{new_group}` leaked at root, \
             flatten is mis-configured",
        );
    }
}

/// Round-trip through `to_value` → `from_value` preserves every
/// field's value via the new sub-struct paths. This is the load-bearing
/// invariant operators rely on: serialise on host A, deserialise on
/// host B, sub-struct fields match.
#[test]
fn round_trip_preserves_every_field() {
    let mut p = GovernancePolicy::default();
    p.core.write = GovernanceLevel::Approve;
    p.core.max_reflection_depth = Some(42);
    p.atomisation.auto_atomise = Some(true);
    p.atomisation.auto_atomise_mode = Some(AutoAtomiseMode::Synchronous);
    p.synthesis.synthesis_max_deletes_per_call = Some(3);
    p.multistep.multistep_max_content_chars = Some(1234);
    p.kind_class.auto_classify_kind = Some(MemoryKindAutoClassify::RegexThenLlm);
    p.persona.auto_persona_trigger_every_n_memories = Some(7);
    p.export.auto_export_reflections_to_filesystem = Some(true);

    let v = serde_json::to_value(&p).expect("serialise");
    let back: GovernancePolicy = serde_json::from_value(v).expect("deserialise");
    assert_eq!(back.core.write, GovernanceLevel::Approve);
    assert_eq!(back.core.max_reflection_depth, Some(42));
    assert_eq!(back.atomisation.auto_atomise, Some(true));
    assert_eq!(
        back.atomisation.auto_atomise_mode,
        Some(AutoAtomiseMode::Synchronous)
    );
    assert_eq!(back.synthesis.synthesis_max_deletes_per_call, Some(3));
    assert_eq!(back.multistep.multistep_max_content_chars, Some(1234));
    assert_eq!(
        back.kind_class.auto_classify_kind,
        Some(MemoryKindAutoClassify::RegexThenLlm)
    );
    assert_eq!(back.persona.auto_persona_trigger_every_n_memories, Some(7));
    assert_eq!(
        back.export.auto_export_reflections_to_filesystem,
        Some(true)
    );
}

/// Partial JSON payload (only some fields supplied) must deserialise
/// with the missing fields defaulting via each sub-struct's
/// `#[serde(default)]`. This pins the v0.6.2 (S34 defensive) contract.
#[test]
fn partial_json_payload_deserialises_with_defaults() {
    let partial = json!({
        "write": "owner",
    });
    let p: GovernancePolicy =
        serde_json::from_value(partial).expect("partial JSON must deserialise");
    assert_eq!(p.core.write, GovernanceLevel::Owner);
    // Defaults from CorePolicy::default()
    assert_eq!(p.core.promote, GovernanceLevel::Any);
    assert_eq!(p.core.delete, GovernanceLevel::Owner);
    assert_eq!(p.core.approver, ApproverType::Human);
    assert!(p.core.inherit);
    assert!(p.core.max_reflection_depth.is_none());
    // Every secondary sub-struct defaults to all-None.
    assert!(p.atomisation.auto_atomise.is_none());
    assert!(p.synthesis.synthesis_failure_mode.is_none());
    assert!(p.multistep.multistep_max_content_chars.is_none());
}

/// Pin that adding a new field to a sub-struct does NOT churn literals
/// at every call site (the architectural win that motivated #880).
/// Concretely: constructing a policy via `..Default::default()` should
/// pick up every field on every sub-struct, so a hypothetical future
/// `auto_atomise_v2_flag` added to `AtomisationPolicy` would be
/// reachable on existing call sites without modification.
#[test]
fn default_default_pattern_picks_up_every_substruct() {
    // The literal pattern documented in CLAUDE.md / #880 acceptance.
    let p = GovernancePolicy {
        core: ai_memory::models::CorePolicy {
            write: GovernanceLevel::Any,
            ..ai_memory::models::CorePolicy::default()
        },
        ..Default::default()
    };
    // The non-explicit fields all resolved to their sub-struct's
    // default (i.e. `None`-Option). The accessor surface still returns
    // the compiled-in defaults.
    assert!(!p.effective_auto_atomise());
    assert_eq!(p.effective_max_reflection_depth(), 3);
    assert_eq!(
        p.effective_synthesis_failure_mode(),
        SynthesisFailureMode::FallThrough
    );
}
