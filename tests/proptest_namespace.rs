// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use ai_memory::models::*;
use proptest::prelude::*;

// Property 1: namespace_depth returns 0 for empty string
proptest! {
    #[test]
    fn prop_namespace_depth_empty(_u in Just("")) {
        assert_eq!(namespace_depth(""), 0);
    }
}

// Property 2: namespace_depth equals segment count for valid paths
proptest! {
    #[test]
    fn prop_namespace_depth_segment_count(
        segments in prop::collection::vec("[a-z0-9_-]{1,50}", 1..8)
    ) {
        let ns = segments.join("/");
        let depth = namespace_depth(&ns);
        assert_eq!(depth, segments.len());
    }
}

// Property 3: namespace_depth("flat") == 1
proptest! {
    #[test]
    fn prop_namespace_depth_flat(s in r"[a-z0-9_-]{1,50}") {
        assert_eq!(namespace_depth(&s), 1);
    }
}

// Property 4: namespace_depth scales with nesting
proptest! {
    #[test]
    fn prop_namespace_depth_scales(depth_target in 1usize..8usize) {
        let mut parts: Vec<&str> = vec![];
        for _i in 0..depth_target {
            parts.push("ns");
        }
        let ns = parts.join("/");
        assert_eq!(namespace_depth(&ns), depth_target);
    }
}

// Property 5: namespace_parent returns None for flat namespaces
proptest! {
    #[test]
    fn prop_namespace_parent_flat_is_none(s in r"[a-z0-9_-]{1,50}") {
        assert_eq!(namespace_parent(&s), None);
    }
}

// Property 6: namespace_parent returns Some for hierarchical paths
proptest! {
    #[test]
    fn prop_namespace_parent_hierarchical(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 2..8)
    ) {
        let ns = segments.join("/");
        if !ns.is_empty() {
            let parent = namespace_parent(&ns);
            if segments.len() > 1 {
                assert!(parent.is_some());
            } else {
                assert_eq!(parent, None);
            }
        }
    }
}

// Property 7: namespace_parent of "a/b/c" is "a/b"
proptest! {
    #[test]
    fn prop_namespace_parent_removes_last_segment(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 2..6)
    ) {
        let ns = segments.join("/");
        if let Some(parent) = namespace_parent(&ns) {
            let expected_parent = segments[..segments.len()-1].join("/");
            assert_eq!(parent, expected_parent);
        }
    }
}

// Property 8: namespace_parent empty string returns None
proptest! {
    #[test]
    fn prop_namespace_parent_empty(_u in Just("")) {
        assert_eq!(namespace_parent(""), None);
    }
}

// Property 9: namespace_ancestors returns non-empty vec for valid namespaces
proptest! {
    #[test]
    fn prop_namespace_ancestors_non_empty(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 1..8)
    ) {
        let ns = segments.join("/");
        let ancestors = namespace_ancestors(&ns);
        assert!(!ancestors.is_empty());
    }
}

// Property 10: namespace_ancestors[0] is the input namespace
proptest! {
    #[test]
    fn prop_namespace_ancestors_first_is_self(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 1..8)
    ) {
        let ns = segments.join("/");
        let ancestors = namespace_ancestors(&ns);
        assert_eq!(ancestors[0], ns);
    }
}

// Property 11: namespace_ancestors length equals depth
proptest! {
    #[test]
    fn prop_namespace_ancestors_length_equals_depth(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 1..8)
    ) {
        let ns = segments.join("/");
        let depth = namespace_depth(&ns);
        let ancestors = namespace_ancestors(&ns);
        assert_eq!(ancestors.len(), depth);
    }
}

// Property 12: namespace_ancestors returns singleton vec for flat namespace
proptest! {
    #[test]
    fn prop_namespace_ancestors_flat_singleton(s in r"[a-z0-9_-]{1,50}") {
        let ancestors = namespace_ancestors(&s);
        assert_eq!(ancestors.len(), 1);
        assert_eq!(ancestors[0], s);
    }
}

// Property 13: namespace_ancestors is ordered most-specific-first
proptest! {
    #[test]
    fn prop_namespace_ancestors_ordered_correctly(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 2..6)
    ) {
        let ns = segments.join("/");
        let ancestors = namespace_ancestors(&ns);

        // Build expected ancestors: [full, parent, grandparent, ...]
        let mut expected: Vec<String> = vec![ns.clone()];
        let mut current = ns.clone();
        while let Some(parent) = namespace_parent(&current) {
            expected.push(parent.clone());
            current = parent;
        }

        assert_eq!(ancestors, expected);
    }
}

// Property 14: namespace_ancestors returns empty vec for empty string
proptest! {
    #[test]
    fn prop_namespace_ancestors_empty_is_empty(_u in Just("")) {
        assert!(namespace_ancestors("").is_empty());
    }
}

// Property 15: Each ancestor in the list exists as parent of previous
proptest! {
    #[test]
    fn prop_namespace_ancestors_parent_chain(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 3..6)
    ) {
        let ns = segments.join("/");
        let ancestors = namespace_ancestors(&ns);

        // Each ancestor should be a parent of the previous one
        for i in 0..ancestors.len()-1 {
            let parent_of_current = namespace_parent(&ancestors[i]);
            assert_eq!(parent_of_current, Some(ancestors[i+1].clone()));
        }
    }
}

// Property 16: Ancestors roundtrip correctly
proptest! {
    #[test]
    fn prop_namespace_ancestors_roundtrip(
        segments in prop::collection::vec("[a-z0-9_-]{1,20}", 1..5)
    ) {
        let ns = segments.join("/");
        let ancestors = namespace_ancestors(&ns);

        // First ancestor should always be the input
        if !ancestors.is_empty() {
            assert_eq!(ancestors[0], ns);

            // Parent of first ancestor should be second (if exists)
            if ancestors.len() > 1 {
                assert_eq!(namespace_parent(&ancestors[0]), Some(ancestors[1].clone()));
            }
        }
    }
}
