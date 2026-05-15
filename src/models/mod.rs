// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use serde_json::Value;

pub mod audit;
pub mod link;
pub mod memory;
pub mod namespace;
pub mod reflection;
pub mod skill;
pub mod tag;

#[allow(unused_imports)]
pub use audit::*;
pub use link::*;
pub use memory::*;
pub use namespace::*;
#[allow(unused_imports)]
pub use reflection::*;
#[allow(unused_imports)]
pub use tag::*;

pub const MAX_CONTENT_SIZE: usize = 65_536;

pub const PROMOTION_THRESHOLD: i64 = 5;
/// How much to extend TTL on access (1 hour for short, 1 day for mid)
pub const SHORT_TTL_EXTEND_SECS: i64 = 3600;
pub const MID_TTL_EXTEND_SECS: i64 = 86400;

pub fn default_metadata() -> Value {
    Value::Object(serde_json::Map::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_from_str_valid() {
        assert_eq!(Tier::from_str("short"), Some(Tier::Short));
        assert_eq!(Tier::from_str("mid"), Some(Tier::Mid));
        assert_eq!(Tier::from_str("long"), Some(Tier::Long));
    }

    #[test]
    fn tier_from_str_invalid() {
        assert_eq!(Tier::from_str("invalid"), None);
        assert_eq!(Tier::from_str(""), None);
        assert_eq!(Tier::from_str("SHORT"), None); // case-sensitive
    }

    #[test]
    fn tier_as_str_roundtrip() {
        for tier in [Tier::Short, Tier::Mid, Tier::Long] {
            let s = tier.as_str();
            assert_eq!(Tier::from_str(s), Some(tier));
        }
    }

    #[test]
    fn tier_default_ttl() {
        assert_eq!(Tier::Short.default_ttl_secs(), Some(6 * 3600));
        assert_eq!(Tier::Mid.default_ttl_secs(), Some(7 * 24 * 3600));
        assert_eq!(Tier::Long.default_ttl_secs(), None);
    }

    #[test]
    fn tier_display() {
        assert_eq!(format!("{}", Tier::Short), "short");
        assert_eq!(format!("{}", Tier::Mid), "mid");
        assert_eq!(format!("{}", Tier::Long), "long");
    }

    #[test]
    fn constants_valid() {
        const _: () = assert!(MAX_CONTENT_SIZE > 0);
        const _: () = assert!(PROMOTION_THRESHOLD > 0);
        assert_eq!(SHORT_TTL_EXTEND_SECS, 3600);
        assert_eq!(MID_TTL_EXTEND_SECS, 86400);
    }

    #[test]
    fn tier_rank_ordering() {
        assert!(Tier::Short.rank() < Tier::Mid.rank());
        assert!(Tier::Mid.rank() < Tier::Long.rank());
        assert_eq!(Tier::Short.rank(), 0);
        assert_eq!(Tier::Mid.rank(), 1);
        assert_eq!(Tier::Long.rank(), 2);
    }

    // ---- v0.7 Track H4 — AttestLevel enum -----------------------------------

    #[test]
    fn attest_level_from_str_canonical_strings() {
        // The three strings H2/H3 already write to the
        // `memory_links.attest_level` column must round-trip.
        assert_eq!(
            AttestLevel::from_str("unsigned"),
            Some(AttestLevel::Unsigned)
        );
        assert_eq!(
            AttestLevel::from_str("self_signed"),
            Some(AttestLevel::SelfSigned)
        );
        assert_eq!(
            AttestLevel::from_str("peer_attested"),
            Some(AttestLevel::PeerAttested)
        );
    }

    #[test]
    fn attest_level_from_str_unknown_returns_none() {
        assert_eq!(AttestLevel::from_str(""), None);
        assert_eq!(AttestLevel::from_str("Unsigned"), None); // case-sensitive
        assert_eq!(AttestLevel::from_str("self-signed"), None); // hyphen wrong
        assert_eq!(AttestLevel::from_str("attested"), None);
    }

    #[test]
    fn attest_level_as_str_round_trips_through_from_str() {
        for lvl in [
            AttestLevel::Unsigned,
            AttestLevel::SelfSigned,
            AttestLevel::PeerAttested,
        ] {
            let s = lvl.as_str();
            assert_eq!(
                AttestLevel::from_str(s),
                Some(lvl),
                "round-trip failed for {lvl:?}"
            );
        }
    }

    #[test]
    fn attest_level_display_matches_as_str() {
        assert_eq!(format!("{}", AttestLevel::Unsigned), "unsigned");
        assert_eq!(format!("{}", AttestLevel::SelfSigned), "self_signed");
        assert_eq!(format!("{}", AttestLevel::PeerAttested), "peer_attested");
    }

    #[test]
    fn attest_level_serde_wire_shape_matches_db_column() {
        // Wire shape = the literal column value. If this drifts, H2/H3
        // outputs and H4 inputs decouple silently.
        let json = serde_json::to_string(&AttestLevel::PeerAttested).unwrap();
        assert_eq!(json, "\"peer_attested\"");
        let back: AttestLevel = serde_json::from_str("\"self_signed\"").unwrap();
        assert_eq!(back, AttestLevel::SelfSigned);
        // Unknown string must fail deserialization (closed-set enum).
        assert!(serde_json::from_str::<AttestLevel>("\"bogus\"").is_err());
    }

    // Task 1.4 — hierarchical namespace helpers --------------------------------

    #[test]
    fn depth_flat_namespace() {
        assert_eq!(namespace_depth("global"), 1);
        assert_eq!(namespace_depth("ai-memory"), 1);
        assert_eq!(namespace_depth("under_score"), 1);
    }

    #[test]
    fn depth_hierarchical() {
        assert_eq!(namespace_depth("a/b"), 2);
        assert_eq!(namespace_depth("alphaone/engineering"), 2);
        assert_eq!(namespace_depth("alphaone/engineering/platform"), 3);
        assert_eq!(
            namespace_depth("a/b/c/d/e/f/g/h"),
            8,
            "max depth of 8 counts each segment"
        );
    }

    #[test]
    fn depth_empty_is_zero() {
        assert_eq!(namespace_depth(""), 0);
    }

    #[test]
    fn parent_hierarchical() {
        assert_eq!(
            namespace_parent("alphaone/engineering/platform"),
            Some("alphaone/engineering".to_string())
        );
        assert_eq!(
            namespace_parent("alphaone/engineering"),
            Some("alphaone".to_string())
        );
    }

    #[test]
    fn parent_flat_is_none() {
        assert_eq!(namespace_parent("global"), None);
        assert_eq!(namespace_parent("ai-memory"), None);
        assert_eq!(namespace_parent(""), None);
    }

    #[test]
    fn ancestors_three_levels() {
        let a = namespace_ancestors("alphaone/engineering/platform");
        assert_eq!(
            a,
            vec![
                "alphaone/engineering/platform".to_string(),
                "alphaone/engineering".to_string(),
                "alphaone".to_string(),
            ],
            "ancestors ordered most-specific-first"
        );
    }

    #[test]
    fn ancestors_flat_namespace() {
        assert_eq!(namespace_ancestors("global"), vec!["global".to_string()]);
        assert_eq!(
            namespace_ancestors("ai-memory"),
            vec!["ai-memory".to_string()]
        );
    }

    #[test]
    fn ancestors_empty_input() {
        assert!(namespace_ancestors("").is_empty());
    }

    #[test]
    fn ancestors_single_level() {
        assert_eq!(namespace_ancestors("a"), vec!["a".to_string()]);
    }

    #[test]
    fn ancestors_max_depth() {
        let a = namespace_ancestors("a/b/c/d/e/f/g/h");
        assert_eq!(a.len(), 8);
        assert_eq!(a[0], "a/b/c/d/e/f/g/h");
        assert_eq!(a[7], "a");
    }

    // Task 1.8 — governance types ---------------------------------------

    #[test]
    fn governance_default_policy() {
        let p = GovernancePolicy::default();
        assert_eq!(p.write, GovernanceLevel::Any);
        assert_eq!(p.promote, GovernanceLevel::Any);
        assert_eq!(p.delete, GovernanceLevel::Owner);
        assert_eq!(p.approver, ApproverType::Human);
        // v0.6.3.1 (P4, G1): inheritance is the documented default. Existing
        // rows are backfilled to true by migration 0012; new rows that omit
        // the field deserialize as true via #[serde(default)].
        assert!(p.inherit);
    }

    #[test]
    fn governance_inherit_field_defaults_true_on_partial_payload() {
        // P4 (G1): a partial-policy payload that omits `inherit` must
        // default to true so legacy callers don't accidentally opt out
        // of parent inheritance the moment they write a child policy.
        let json = r#"{"write":"approve"}"#;
        let p: GovernancePolicy = serde_json::from_str(json).unwrap();
        assert_eq!(p.write, GovernanceLevel::Approve);
        assert!(p.inherit, "missing `inherit` must deserialize as true");
    }

    #[test]
    fn governance_inherit_field_explicit_false_round_trip() {
        // P4 (G1): when an operator explicitly opts a subtree out of
        // inheritance, the false value must round-trip and serialize.
        let json = r#"{"write":"any","inherit":false}"#;
        let p: GovernancePolicy = serde_json::from_str(json).unwrap();
        assert!(!p.inherit);
        let back = serde_json::to_value(&p).unwrap();
        assert_eq!(back["inherit"], false);
    }

    #[test]
    fn governance_level_serde_snake_case() {
        // Serialize each level as a lowercase JSON string
        for (level, expected) in [
            (GovernanceLevel::Any, "any"),
            (GovernanceLevel::Registered, "registered"),
            (GovernanceLevel::Owner, "owner"),
            (GovernanceLevel::Approve, "approve"),
        ] {
            let json = serde_json::to_string(&level).unwrap();
            assert_eq!(json, format!("\"{expected}\""));
            // Roundtrip
            let back: GovernanceLevel = serde_json::from_str(&json).unwrap();
            assert_eq!(back, level);
        }
    }

    #[test]
    fn approver_type_serde_shapes() {
        // Human → unit variant serializes as bare string
        let json = serde_json::to_string(&ApproverType::Human).unwrap();
        assert_eq!(json, "\"human\"");

        // Agent(s) → externally tagged
        let a = ApproverType::Agent("alice".to_string());
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(json, r#"{"agent":"alice"}"#);
        let back: ApproverType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, a);

        // Consensus(n) → externally tagged, numeric payload
        let c = ApproverType::Consensus(3);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, r#"{"consensus":3}"#);
        let back: ApproverType = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn governance_policy_full_roundtrip() {
        let p = GovernancePolicy {
            write: GovernanceLevel::Registered,
            promote: GovernanceLevel::Approve,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Agent("maintainer".to_string()),
            inherit: true,
            max_reflection_depth: None,
            auto_export_reflections_to_filesystem: None,
            auto_atomise: None,
            auto_atomise_threshold_cl100k: None,
            auto_atomise_max_atom_tokens: None,
            auto_persona_trigger_every_n_memories: None,
            auto_export_personas_to_filesystem: None,
            auto_atomise_mode: None,
            legacy_per_pair_classifier: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: GovernancePolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn governance_from_metadata_missing() {
        let meta = serde_json::json!({"agent_id": "alice"});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn governance_from_metadata_null() {
        let meta = serde_json::json!({"governance": null});
        assert!(GovernancePolicy::from_metadata(&meta).is_none());
    }

    #[test]
    fn governance_from_metadata_default_shape() {
        let default = GovernancePolicy::default();
        let meta = serde_json::json!({"governance": serde_json::to_value(&default).unwrap()});
        let parsed = GovernancePolicy::from_metadata(&meta)
            .expect("present")
            .expect("valid");
        assert_eq!(parsed, default);
    }

    #[test]
    fn governance_from_metadata_invalid_returns_err() {
        let meta = serde_json::json!({
            "governance": {"write": "bogus", "promote": "any", "delete": "any", "approver": "human"}
        });
        let result = GovernancePolicy::from_metadata(&meta).expect("present");
        assert!(result.is_err(), "unknown enum value must fail deserialize");
    }

    // v0.6.2 (S34 defense): partial policy payloads fall back to the
    // `Default for GovernancePolicy` values for any field the caller omitted.
    // `write` remains required — it's the core knob the policy expresses.

    #[test]
    fn governance_partial_policy_write_only_uses_defaults() {
        let json = serde_json::json!({"write": "owner"});
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("write-only parses");
        assert_eq!(parsed.write, GovernanceLevel::Owner);
        assert_eq!(parsed.promote, GovernanceLevel::Any);
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Human);
    }

    #[test]
    fn governance_partial_policy_write_and_promote() {
        let json = serde_json::json!({"write": "any", "promote": "registered"});
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.promote, GovernanceLevel::Registered);
        // Absent fields still take defaults.
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Human);
    }

    #[test]
    fn governance_missing_write_still_errors() {
        // `write` is the core policy knob — must remain required to avoid
        // silently accepting an empty object as "any writes allowed".
        let json = serde_json::json!({"promote": "owner"});
        let err = serde_json::from_value::<GovernancePolicy>(json);
        assert!(err.is_err(), "missing write must fail deserialize");
    }

    #[test]
    fn governance_level_as_str_tags() {
        assert_eq!(GovernanceLevel::Any.as_str(), "any");
        assert_eq!(GovernanceLevel::Registered.as_str(), "registered");
        assert_eq!(GovernanceLevel::Owner.as_str(), "owner");
        assert_eq!(GovernanceLevel::Approve.as_str(), "approve");
    }

    #[test]
    fn approver_type_kind_tags() {
        assert_eq!(ApproverType::Human.kind(), "human");
        assert_eq!(ApproverType::Agent("a".into()).kind(), "agent");
        assert_eq!(ApproverType::Consensus(3).kind(), "consensus");
    }

    // -----------------------------------------------------------------
    // W12-H — additional small-module pinning
    // -----------------------------------------------------------------

    #[test]
    fn default_metadata_is_empty_object() {
        let v = default_metadata();
        assert!(v.is_object());
        assert!(v.as_object().unwrap().is_empty());
    }

    #[test]
    fn governed_action_as_str_pinned() {
        assert_eq!(GovernedAction::Store.as_str(), "store");
        assert_eq!(GovernedAction::Delete.as_str(), "delete");
        assert_eq!(GovernedAction::Promote.as_str(), "promote");
    }

    #[test]
    fn governance_decision_equality() {
        assert_eq!(GovernanceDecision::Allow, GovernanceDecision::Allow);
        assert_ne!(
            GovernanceDecision::Deny("a".into()),
            GovernanceDecision::Deny("b".into()),
        );
        assert_eq!(
            GovernanceDecision::Pending("p1".into()),
            GovernanceDecision::Pending("p1".into())
        );
    }

    #[test]
    fn vector_clock_observe_monotonic() {
        let mut vc = VectorClock::default();
        vc.observe("peer-a", "2026-04-01T00:00:00+00:00");
        vc.observe("peer-a", "2026-05-01T00:00:00+00:00");
        // Older never overwrites newer.
        vc.observe("peer-a", "2026-03-01T00:00:00+00:00");
        assert_eq!(vc.latest_from("peer-a"), Some("2026-05-01T00:00:00+00:00"));
    }

    #[test]
    fn vector_clock_latest_from_unknown_is_none() {
        let vc = VectorClock::default();
        assert!(vc.latest_from("never-seen").is_none());
    }

    #[test]
    fn vector_clock_serde_roundtrip() {
        let mut vc = VectorClock::default();
        vc.observe("p1", "2026-04-01T00:00:00+00:00");
        vc.observe("p2", "2026-04-02T00:00:00+00:00");
        let json = serde_json::to_string(&vc).unwrap();
        let back: VectorClock = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back, vc);
    }

    #[test]
    fn namespace_parent_with_trailing_slash() {
        // "a/" splits to parent="a" and tail="". The function returns the
        // parent regardless of whether the final segment is empty.
        assert_eq!(namespace_parent("a/"), Some("a".to_string()));
    }

    #[test]
    fn namespace_depth_skips_empty_segments() {
        // Multiple slashes do not inflate the depth count.
        assert_eq!(namespace_depth("a//b"), 2);
        assert_eq!(namespace_depth("/a"), 1);
        assert_eq!(namespace_depth("a/"), 1);
    }

    #[test]
    fn namespace_ancestors_two_levels() {
        // Two-level namespace produces self + parent.
        assert_eq!(
            namespace_ancestors("a/b"),
            vec!["a/b".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn memory_serde_roundtrip_minimal() {
        let m = Memory {
            id: "abc".into(),
            tier: Tier::Mid,
            namespace: "global".into(),
            title: "t".into(),
            content: "c".into(),
            tags: vec!["x".into()],
            priority: 5,
            confidence: 0.9,
            source: "api".into(),
            access_count: 0,
            created_at: "2026-04-01T00:00:00+00:00".into(),
            updated_at: "2026-04-01T00:00:00+00:00".into(),
            last_accessed_at: None,
            expires_at: None,
            metadata: default_metadata(),
            reflection_depth: 0,
            memory_kind: MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: Memory = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, m.id);
        assert_eq!(back.tier, Tier::Mid);
    }

    #[test]
    fn approver_type_kind_for_each_variant() {
        // Hits all three discriminant arms. Mirrors the existing test but
        // ensures we cover a Consensus(0) which is the lower edge.
        assert_eq!(ApproverType::Human.kind(), "human");
        assert_eq!(ApproverType::Agent(String::new()).kind(), "agent");
        assert_eq!(ApproverType::Consensus(0).kind(), "consensus");
    }

    #[test]
    fn governance_partial_policy_with_approver() {
        // Partial policy with `approver` set and other fields defaulted.
        let json = serde_json::json!({
            "write": "owner",
            "approver": {"agent": "alice"}
        });
        let parsed: GovernancePolicy = serde_json::from_value(json).expect("parses");
        assert_eq!(parsed.write, GovernanceLevel::Owner);
        assert_eq!(parsed.approver, ApproverType::Agent("alice".to_string()));
        assert_eq!(parsed.promote, GovernanceLevel::Any);
        assert_eq!(parsed.delete, GovernanceLevel::Owner);
    }
}
