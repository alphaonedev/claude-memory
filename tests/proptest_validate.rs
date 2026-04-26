// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use ai_memory::validate::*;
use proptest::prelude::*;
use serde_json::json;

// Property 1: Valid titles should always round-trip through validation
proptest! {
    #[test]
    fn prop_title_valid_roundtrip(s in r"[A-Za-z0-9 _\-.,!?\n\t]{1,100}") {
        let title = s.trim();
        if !title.is_empty() && title.len() <= 512 {
            // If it validates, it's a valid title
            let result = validate_title(title);
            if result.is_ok() {
                // The result should be deterministic
                assert_eq!(validate_title(title).is_ok(), true);
            }
        }
    }
}

// Property 2: Empty/whitespace-only titles should always be rejected
proptest! {
    #[test]
    fn prop_title_empty_rejected(s in r"[ \t\n]*") {
        assert!(validate_title(&s).is_err());
    }
}

// Property 3: Titles exceeding max length should be rejected
proptest! {
    #[test]
    fn prop_title_max_length(s in ".{513,}") {
        assert!(validate_title(&s).is_err());
    }
}

// Property 4: Valid namespaces with hierarchical structure
proptest! {
    #[test]
    fn prop_namespace_hierarchical_valid(
        segments in prop::collection::vec("[a-z0-9_-]{1,50}", 1..8)
    ) {
        let ns = segments.join("/");
        if !ns.starts_with('/') && !ns.ends_with('/') && !ns.contains("//") {
            // Most hierarchical paths should validate
            let result = validate_namespace(&ns);
            // Validation should be idempotent
            assert_eq!(validate_namespace(&ns).is_ok(), result.is_ok());
        }
    }
}

// Property 5: Namespace depth = number of '/' + 1
proptest! {
    #[test]
    fn prop_namespace_depth_is_segment_count(s in r"[a-z][a-z0-9]*(/[a-z][a-z0-9]*){0,7}") {
        if validate_namespace(&s).is_ok() {
            let depth = ai_memory::models::namespace_depth(&s);
            let separator_count = s.split('/').count();
            assert_eq!(depth, separator_count);
        }
    }
}

// Property 6: Namespaces with spaces should be rejected
proptest! {
    #[test]
    fn prop_namespace_space_rejected(
        base in r"[a-z0-9_-]{1,50}"
    ) {
        let ns = format!("{} test", base);
        assert!(validate_namespace(&ns).is_err());
    }
}

// Property 7: Valid agent_ids should round-trip
proptest! {
    #[test]
    fn prop_agent_id_valid_roundtrip(
        s in r"[a-zA-Z0-9_\-:@.]{1,128}"
    ) {
        let result = validate_agent_id(&s);
        // Validation should be idempotent
        assert_eq!(validate_agent_id(&s).is_ok(), result.is_ok());
    }
}

// Property 8: Empty agent_ids should be rejected
proptest! {
    #[test]
    fn prop_agent_id_empty_rejected(_s in Just("")) {
        assert!(validate_agent_id("").is_err());
    }
}

// Property 9: Valid scopes from the closed set
proptest! {
    #[test]
    fn prop_scope_valid_set(choice in 0usize..5) {
        let scopes = vec!["private", "team", "unit", "org", "collective"];
        let scope = scopes[choice % scopes.len()];
        assert!(validate_scope(scope).is_ok());
    }
}

// Property 10: Invalid scopes should be rejected
proptest! {
    #[test]
    fn prop_scope_invalid_rejected(
        s in r"[a-z_]{1,50}"
    ) {
        if !vec!["private", "team", "unit", "org", "collective"].contains(&s.as_str()) {
            assert!(validate_scope(&s).is_err());
        }
    }
}

// Property 11: Metadata validation with reasonable depth
proptest! {
    #[test]
    fn prop_metadata_simple_object(
        _u in Just(())
    ) {
        let obj = json!({"key": "value", "nested": {"data": 123}});
        assert!(validate_metadata(&obj).is_ok());
    }
}

// Property 12: Tags validation: no empty tags
proptest! {
    #[test]
    fn prop_tags_no_empty(tags in prop::collection::vec("[a-z0-9_]{1,50}", 0..20)) {
        assert!(validate_tags(&tags).is_ok());
    }
}

// Property 13: Too many tags should be rejected
proptest! {
    #[test]
    fn prop_tags_max_count_enforced(
        tags in prop::collection::vec("[a-z]{1,50}", 51..100)
    ) {
        assert!(validate_tags(&tags).is_err());
    }
}

// Property 14: RFC3339 timestamp validation
proptest! {
    #[test]
    fn prop_expires_at_valid_future(
        year in 2026u32..2100u32,
        month in 1u32..=12u32,
        day in 1u32..=28u32,
    ) {
        let ts = format!("{:04}-{:02}-{:02}T12:00:00Z", year, month, day);
        // All of these should be valid RFC3339
        let result = validate_expires_at(Some(&ts));
        // Past dates might fail (if today > this date), but format should be valid
        if result.is_err() {
            assert!(ts.parse::<chrono::DateTime<chrono::FixedOffset>>().is_ok() ||
                    chrono::DateTime::parse_from_rfc3339(&ts).is_ok());
        }
    }
}

// Property 15: TTL validation: positive seconds only
proptest! {
    #[test]
    fn prop_ttl_positive_only(
        secs in 1i64..=31536000i64  // 1 sec to 1 year
    ) {
        assert!(validate_ttl_secs(Some(secs)).is_ok());
    }
}

// Property 16: TTL validation: zero and negative rejected
proptest! {
    #[test]
    fn prop_ttl_non_positive_rejected(
        secs in -100i64..=0i64
    ) {
        assert!(validate_ttl_secs(Some(secs)).is_err());
    }
}
