// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! Issue #655 Task 2/8 — `max_reflection_depth` namespace governance field.
//!
//! v0.7.0 add-on mission, recursive learning, Task 2/8. Pins the
//! per-namespace cap a future `memory_reflect` MCP write path
//! (Task 4/8 wires the tool; Task 5/8 wires the substrate-side
//! refusal) will consult to decide whether to accept a reflection
//! write at depth N.
//!
//! Surface pinned here:
//!   - `GovernancePolicy::max_reflection_depth: Option<u32>` — the
//!     new optional override field on the namespace governance
//!     JSON struct in `src/models.rs`.
//!   - `GovernancePolicy::effective_max_reflection_depth(&self) -> u32`
//!     — the accessor that resolves `None → 3` (compiled default).
//!
//! Contracts:
//!   - **Default behavior**: a namespace whose governance JSON omits
//!     `max_reflection_depth` resolves to `3`. Default of 3 bounds
//!     recursion (reflection-on-reflection-on-…) without strangling
//!     the legitimate reflection-on-reflection chains the v0.8.0
//!     Pillar 2.5 curator mode will lean on.
//!   - **Explicit override**: `Some(N)` returns exactly `N`.
//!   - **Zero disables**: `Some(0)` returns `0`. Task 5/8 enforces
//!     `proposed_reflection_depth >= cap → refuse`, so a cap of 0
//!     refuses every reflection (no depth `>= 0` passes). This is
//!     the documented kill-switch for a namespace that should never
//!     accept reflection writes.
//!   - **Wire-shape backwards compatibility**: a pre-v0.7.0 namespace
//!     JSON literal that omits the field deserializes cleanly with
//!     the field as `None`. `skip_serializing_if = "Option::is_none"`
//!     keeps the absent shape on the wire so federation /
//!     replication payloads stay byte-identical for legacy peers.
//!   - **Ancestor inheritance is intentionally NOT applied at the
//!     accessor level.** The leaf-first ancestor walk lives at
//!     `db::resolve_governance_policy` (and the equivalent
//!     `MemoryStore::resolve_governance_policy` trait method); that
//!     resolver returns the most-specific `GovernancePolicy` for the
//!     namespace chain, and the caller then calls
//!     `effective_max_reflection_depth()` on the result. Putting
//!     a second ancestor-walk inside the accessor would double-walk
//!     and is structurally redundant. We pin that the accessor is
//!     flat here so the future Task 5/8 enforcement path has a
//!     stable contract.
//!
//! No schema migration is required. The field lives inside the
//! existing `metadata.governance` JSON blob persisted on the
//! namespace's standard memory (the storage column is `TEXT` on
//! SQLite and `JSONB` on Postgres — both already accept additive
//! keys). Accordingly this test file does NOT bump
//! `SCHEMA_VERSION` / `CURRENT_SCHEMA_VERSION` /
//! `MAX_SUPPORTED_SCHEMA`.

use ai_memory::models::{ApproverType, GovernanceLevel, GovernancePolicy};

// ─────────────────────────────────────────────────────────────────────
// Accessor — default behavior and overrides.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn effective_max_reflection_depth_default_is_three() {
    // `GovernancePolicy::default()` leaves the override absent, so
    // the accessor must return the compiled-in default of 3. This is
    // the v0.7.0 baseline contract that Task 5/8 enforcement reads
    // against when no operator override exists.
    let p = GovernancePolicy::default();
    assert_eq!(p.max_reflection_depth, None);
    assert_eq!(p.effective_max_reflection_depth(), 3);
}

#[test]
fn effective_max_reflection_depth_managed_namespace_default_is_three() {
    // The "managed namespace" bootstrap (write=Owner) also leaves
    // the override absent — operators get the same compiled-in
    // default of 3 until they explicitly tune it.
    let p = GovernancePolicy::default_for_managed_namespace();
    assert_eq!(p.max_reflection_depth, None);
    assert_eq!(p.effective_max_reflection_depth(), 3);
}

#[test]
fn effective_max_reflection_depth_explicit_override_returns_value() {
    // An operator that wants deeper-than-default reflection chains
    // (e.g. v0.8.0 Pillar 2.5 curator chains) overrides via the
    // optional field. The accessor returns the override verbatim.
    let p = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: Some(7),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
    };
    assert_eq!(p.effective_max_reflection_depth(), 7);
}

#[test]
fn effective_max_reflection_depth_some_zero_disables_reflection() {
    // **Kill-switch contract** — `Some(0)` MUST return `0` so the
    // future Task 5/8 enforcement path (`depth >= cap → refuse`)
    // refuses every reflection in a namespace that has opted out.
    // No depth `>= 0` passes that comparison, so the cap of 0 is
    // the disable-all-reflections sentinel. This test pins the
    // semantic so a future "0 means use default" misreading would
    // be caught immediately.
    let p = GovernancePolicy {
        write: GovernanceLevel::Any,
        promote: GovernanceLevel::Any,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Human,
        inherit: true,
        max_reflection_depth: Some(0),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
    };
    assert_eq!(
        p.effective_max_reflection_depth(),
        0,
        "Some(0) must return 0 so the depth-limit refusal path can disable all reflections"
    );
}

#[test]
fn effective_max_reflection_depth_some_one_returns_one() {
    // Edge case adjacent to the kill-switch: `Some(1)` permits
    // depth=0 base memories to spawn depth=1 reflections, but
    // refuses any deeper chain. This pins the boundary above the
    // disable sentinel.
    let p = GovernancePolicy {
        max_reflection_depth: Some(1),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        ..GovernancePolicy::default()
    };
    assert_eq!(p.effective_max_reflection_depth(), 1);
}

#[test]
fn effective_max_reflection_depth_high_override_returns_value() {
    // The accessor performs no clamp. A namespace that genuinely
    // wants a high cap (e.g. an audit sandbox proving the recursion
    // is stable under load) gets exactly what was configured.
    // Clamping is an enforcement-policy decision — the accessor
    // is a pure resolver.
    let p = GovernancePolicy {
        max_reflection_depth: Some(255),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        ..GovernancePolicy::default()
    };
    assert_eq!(p.effective_max_reflection_depth(), 255);
}

// ─────────────────────────────────────────────────────────────────────
// Wire shape — serde JSON round-trip.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn deserialize_legacy_policy_without_field_defaults_to_none() {
    // Backwards-compatibility contract: a pre-v0.7.0 namespace
    // governance JSON literal that omits `max_reflection_depth`
    // must deserialize cleanly with the field as `None`, and the
    // accessor must fall back to the compiled-in default of 3.
    // This is the load-bearing test for federation / replication —
    // an older peer that doesn't write the field must not poison
    // the receive path on an up-to-date node.
    let legacy = r#"{
        "write": "any",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true
    }"#;
    let p: GovernancePolicy =
        serde_json::from_str(legacy).expect("pre-v0.7.0 governance JSON must deserialize");
    assert_eq!(p.max_reflection_depth, None);
    assert_eq!(p.effective_max_reflection_depth(), 3);
}

#[test]
fn deserialize_partial_legacy_policy_with_write_only_defaults_to_none() {
    // The partial-policy shape (just `{"write": "owner"}`) is a
    // common operator-CLI / test-harness pattern. With `inherit`
    // already on `#[serde(default)]`, a partial payload that omits
    // `max_reflection_depth` must also resolve cleanly. Mirrors
    // `governance_partial_policy_write_only_uses_defaults` in the
    // model-level unit suite.
    let json = serde_json::json!({"write": "owner"});
    let p: GovernancePolicy =
        serde_json::from_value(json).expect("partial governance JSON must deserialize");
    assert_eq!(p.write, GovernanceLevel::Owner);
    assert_eq!(p.max_reflection_depth, None);
    assert_eq!(p.effective_max_reflection_depth(), 3);
}

#[test]
fn deserialize_v0_7_0_policy_with_explicit_field_preserves_value() {
    // A v0.7.0+ governance JSON literal that carries the new field
    // must round-trip verbatim — the deserializer must not silently
    // drop the override.
    let v0_7_0 = r#"{
        "write": "any",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true,
        "max_reflection_depth": 5
    }"#;
    let p: GovernancePolicy =
        serde_json::from_str(v0_7_0).expect("v0.7.0 governance JSON must deserialize");
    assert_eq!(p.max_reflection_depth, Some(5));
    assert_eq!(p.effective_max_reflection_depth(), 5);
}

#[test]
fn deserialize_v0_7_0_policy_with_explicit_zero_preserves_disable_sentinel() {
    // Kill-switch wire shape — `"max_reflection_depth": 0` must
    // arrive as `Some(0)`, not be coerced to `None`. If serde
    // accidentally treated 0 as a "missing" marker the namespace
    // would silently re-open reflections at depth 3.
    let v0_7_0_disabled = r#"{
        "write": "any",
        "promote": "any",
        "delete": "owner",
        "approver": "human",
        "inherit": true,
        "max_reflection_depth": 0
    }"#;
    let p: GovernancePolicy = serde_json::from_str(v0_7_0_disabled)
        .expect("v0.7.0 disabled-reflections governance JSON must deserialize");
    assert_eq!(p.max_reflection_depth, Some(0));
    assert_eq!(p.effective_max_reflection_depth(), 0);
}

#[test]
fn serialize_policy_with_absent_field_omits_key_on_the_wire() {
    // `#[serde(default, skip_serializing_if = "Option::is_none")]`
    // contract — when the override is unset, the JSON output must
    // NOT contain the key at all (not `null`, not the default).
    // This is the wire-shape parity with `NamespaceMetaEntry::parent_namespace`
    // that keeps replication payloads byte-identical for legacy peers
    // that read older shapes.
    let p = GovernancePolicy::default();
    let json = serde_json::to_value(&p).expect("serialize");
    assert!(
        !json
            .as_object()
            .expect("policy serializes as object")
            .contains_key("max_reflection_depth"),
        "absent field must NOT appear in JSON output; got: {json:?}"
    );
}

#[test]
fn serialize_policy_with_explicit_field_writes_key_on_the_wire() {
    // The mirror case — when the override IS set, the JSON output
    // must include the key with the numeric value. Pins that the
    // `skip_serializing_if` is only firing on `None`, not also on
    // `Some(0)` (which would silently drop the disable sentinel).
    let p = GovernancePolicy {
        max_reflection_depth: Some(0),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
        ..GovernancePolicy::default()
    };
    let json = serde_json::to_value(&p).expect("serialize");
    assert_eq!(
        json["max_reflection_depth"], 0,
        "explicit Some(0) must serialize as 0, not be skipped; got: {json:?}"
    );
}

#[test]
fn full_roundtrip_with_explicit_field() {
    // Compound round-trip: build a policy with every field set
    // (including the new one), serialize, deserialize, and confirm
    // bitwise equality. Mirrors the existing
    // `governance_policy_full_roundtrip` test but adds the new
    // field to the matrix.
    let p = GovernancePolicy {
        write: GovernanceLevel::Registered,
        promote: GovernanceLevel::Approve,
        delete: GovernanceLevel::Owner,
        approver: ApproverType::Agent("maintainer".to_string()),
        inherit: true,
        max_reflection_depth: Some(4),
        auto_export_reflections_to_filesystem: None,
        auto_atomise: None,
        auto_atomise_threshold_cl100k: None,
        auto_atomise_max_atom_tokens: None,
        auto_persona_trigger_every_n_memories: None,
        auto_export_personas_to_filesystem: None,
        auto_atomise_mode: None,
        legacy_per_pair_classifier: None,
    };
    let json = serde_json::to_string(&p).expect("serialize");
    let back: GovernancePolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, p, "full round-trip must preserve every field");
}

#[test]
fn full_roundtrip_without_explicit_field() {
    // Round-trip parity for the absent-field shape — a policy with
    // `max_reflection_depth: None` must serialize to JSON without
    // the key, and deserializing that JSON must yield a policy
    // bitwise-equal to the original.
    let p = GovernancePolicy::default();
    let json = serde_json::to_string(&p).expect("serialize");
    let back: GovernancePolicy = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, p);
    assert_eq!(back.max_reflection_depth, None);
}

// ─────────────────────────────────────────────────────────────────────
// Metadata-blob shape — the field lives inside `metadata.governance`.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn from_metadata_recognizes_v0_7_0_governance_with_max_reflection_depth() {
    // The canonical persisted shape — the field lives inside
    // `metadata.governance`, which is the JSON blob attached to the
    // namespace's standard memory. `from_metadata` is the reader
    // both SQLite and Postgres adapters call; it must surface the
    // new field without modification.
    let meta = serde_json::json!({
        "governance": {
            "write": "any",
            "promote": "any",
            "delete": "owner",
            "approver": "human",
            "inherit": true,
            "max_reflection_depth": 2
        }
    });
    let parsed = GovernancePolicy::from_metadata(&meta)
        .expect("governance key present")
        .expect("valid governance shape");
    assert_eq!(parsed.max_reflection_depth, Some(2));
    assert_eq!(parsed.effective_max_reflection_depth(), 2);
}

#[test]
fn from_metadata_legacy_governance_without_field_falls_through_to_default() {
    // A pre-v0.7.0 metadata blob (no `max_reflection_depth` inside
    // the `governance` object) must still deserialize, and the
    // resulting policy's accessor must yield the compiled-in
    // default of 3. This is the load-bearing test for the
    // migration-free roll-out: existing rows on disk continue to
    // resolve correctly without backfill.
    let meta = serde_json::json!({
        "governance": {
            "write": "any",
            "promote": "any",
            "delete": "owner",
            "approver": "human",
            "inherit": true
        }
    });
    let parsed = GovernancePolicy::from_metadata(&meta)
        .expect("governance key present")
        .expect("legacy shape must deserialize");
    assert_eq!(parsed.max_reflection_depth, None);
    assert_eq!(parsed.effective_max_reflection_depth(), 3);
}
