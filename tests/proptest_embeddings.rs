// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

use ai_memory::embeddings::Embedder;
use proptest::prelude::*;

// Property 1: cosine_similarity(v, v) == 1.0 (identical vectors)
proptest! {
    #[test]
    fn prop_cosine_self_similarity_is_one(
        v in prop::collection::vec(-1000f32..1000f32, 10..50)
    ) {
        let similarity = Embedder::cosine_similarity(&v, &v);
        // Should be 1.0 unless the vector is all zeros
        if v.iter().any(|x| x.abs() > 1e-6) {
            assert!((similarity - 1.0).abs() < 1e-5);
        }
    }
}

// Property 2: cosine_similarity(v, -v) == -1.0 (opposite vectors)
proptest! {
    #[test]
    fn prop_cosine_opposite_is_minus_one(
        v in prop::collection::vec(-1000f32..1000f32, 10..50)
    ) {
        let neg_v: Vec<f32> = v.iter().map(|x| -x).collect();
        let similarity = Embedder::cosine_similarity(&v, &neg_v);
        // Should be -1.0 unless the vector is all zeros
        if v.iter().any(|x| x.abs() > 1e-6) {
            assert!((similarity + 1.0).abs() < 1e-5);
        }
    }
}

// Property 3: cosine_similarity is symmetric
proptest! {
    #[test]
    fn prop_cosine_symmetric(
        a in prop::collection::vec(-100f32..100f32, 10..30),
        b in prop::collection::vec(-100f32..100f32, 10..30)
    ) {
        if a.len() == b.len() {
            let sim_ab = Embedder::cosine_similarity(&a, &b);
            let sim_ba = Embedder::cosine_similarity(&b, &a);
            assert!((sim_ab - sim_ba).abs() < 1e-6);
        }
    }
}

// Property 4: cosine_similarity bounds are [-1, 1]
proptest! {
    #[test]
    fn prop_cosine_bounded(
        a in prop::collection::vec(-1000f32..1000f32, 5..50),
        b in prop::collection::vec(-1000f32..1000f32, 5..50)
    ) {
        if a.len() == b.len() {
            let similarity = Embedder::cosine_similarity(&a, &b);
            assert!(similarity >= -1.0 && similarity <= 1.0,
                   "cosine similarity {} out of bounds [-1, 1]", similarity);
        }
    }
}

// Property 5: cosine_similarity with zero vector
proptest! {
    #[test]
    fn prop_cosine_zero_vector(
        a in prop::collection::vec(-100f32..100f32, 5..30)
    ) {
        let zero: Vec<f32> = vec![0.0; a.len()];
        let similarity = Embedder::cosine_similarity(&a, &zero);
        // Either both are zero (ill-defined, return 0.0) or similarity is 0
        assert_eq!(similarity, 0.0);
    }
}

// Property 6: cosine_similarity with mismatched dimensions returns 0.0
proptest! {
    #[test]
    fn prop_cosine_mismatched_dims(
        a in prop::collection::vec(-100f32..100f32, 5..30),
        b in prop::collection::vec(-100f32..100f32, 5..30)
    ) {
        if a.len() != b.len() && !a.is_empty() && !b.is_empty() {
            let similarity = Embedder::cosine_similarity(&a, &b);
            assert_eq!(similarity, 0.0);
        }
    }
}

// Property 7: cosine_similarity orthogonal vectors are ~0
proptest! {
    #[test]
    fn prop_cosine_orthogonal_near_zero(dim in 5usize..20usize) {
        // Build two orthogonal vectors: (1, 0, 0, ...) and (0, 1, 0, ...)
        let mut a = vec![0.0; dim];
        let mut b = vec![0.0; dim];
        if dim > 0 {
            a[0] = 1.0;
        }
        if dim > 1 {
            b[1] = 1.0;
        }

        let similarity = Embedder::cosine_similarity(&a, &b);
        // Orthogonal vectors have cosine similarity 0
        assert!(similarity.abs() < 1e-6);
    }
}

// Property 8: cosine_similarity with scaled vectors
proptest! {
    #[test]
    fn prop_cosine_scale_invariant(
        v in prop::collection::vec(1f32..100f32, 5..30),
        scale in 0.1f32..10.0f32
    ) {
        let scaled: Vec<f32> = v.iter().map(|x| x * scale).collect();
        let sim_original = Embedder::cosine_similarity(&v, &v);
        let sim_scaled = Embedder::cosine_similarity(&scaled, &scaled);

        // Both should equal 1.0 (up to floating-point precision)
        if v.iter().any(|x| x.abs() > 1e-6) {
            assert!((sim_original - 1.0).abs() < 1e-5);
            assert!((sim_scaled - 1.0).abs() < 1e-5);
        }
    }
}

// Property 9: cosine_similarity triangle inequality-like behavior
proptest! {
    #[test]
    fn prop_cosine_satisfies_basic_properties(
        a in prop::collection::vec(-10f32..10f32, 5..20),
        b in prop::collection::vec(-10f32..10f32, 5..20),
        c in prop::collection::vec(-10f32..10f32, 5..20)
    ) {
        if a.len() == b.len() && b.len() == c.len() && !a.is_empty() {
            let sim_ab = Embedder::cosine_similarity(&a, &b);
            let sim_bc = Embedder::cosine_similarity(&b, &c);
            let sim_ac = Embedder::cosine_similarity(&a, &c);

            // All similarities should be bounded
            assert!(sim_ab >= -1.0 && sim_ab <= 1.0);
            assert!(sim_bc >= -1.0 && sim_bc <= 1.0);
            assert!(sim_ac >= -1.0 && sim_ac <= 1.0);
        }
    }
}

// Property 10: fuse_weight clamping works correctly
proptest! {
    #[test]
    fn prop_fuse_clamping(
        primary in prop::collection::vec(-10f32..10f32, 10..20),
        secondary in prop::collection::vec(-10f32..10f32, 10..20),
        weight in -2.0f32..3.0f32
    ) {
        if primary.len() == secondary.len() && !primary.is_empty() {
            let fused = Embedder::fuse(&primary, &secondary, weight);

            // Fused should have same length as input
            assert_eq!(fused.len(), primary.len());

            // Each element should be a linear combination (weight clamped to [0,1])
            let clamped_w = weight.clamp(0.0, 1.0);
            for i in 0..fused.len() {
                let expected = clamped_w * primary[i] + (1.0 - clamped_w) * secondary[i];
                assert!((fused[i] - expected).abs() < 1e-5);
            }
        }
    }
}

// Property 11: fuse preserves dimension
proptest! {
    #[test]
    fn prop_fuse_dimension_preserved(
        primary in prop::collection::vec(-100f32..100f32, 5..50),
        secondary in prop::collection::vec(-100f32..100f32, 5..50)
    ) {
        if primary.len() == secondary.len() {
            let fused = Embedder::fuse(&primary, &secondary, 0.5);
            assert_eq!(fused.len(), primary.len());
        }
    }
}

// Property 12: fuse with weight=1.0 returns primary
proptest! {
    #[test]
    fn prop_fuse_weight_one_is_primary(
        primary in prop::collection::vec(-100f32..100f32, 5..30),
        secondary in prop::collection::vec(-100f32..100f32, 5..30)
    ) {
        if primary.len() == secondary.len() {
            let fused = Embedder::fuse(&primary, &secondary, 1.0);
            for i in 0..fused.len() {
                assert!((fused[i] - primary[i]).abs() < 1e-5);
            }
        }
    }
}

// Property 13: fuse with weight=0.0 returns secondary
proptest! {
    #[test]
    fn prop_fuse_weight_zero_is_secondary(
        primary in prop::collection::vec(-100f32..100f32, 5..30),
        secondary in prop::collection::vec(-100f32..100f32, 5..30)
    ) {
        if primary.len() == secondary.len() {
            let fused = Embedder::fuse(&primary, &secondary, 0.0);
            for i in 0..fused.len() {
                assert!((fused[i] - secondary[i]).abs() < 1e-5);
            }
        }
    }
}

// Property 14: fuse with mismatched dimensions falls back to primary
proptest! {
    #[test]
    fn prop_fuse_mismatch_fallback(
        primary in prop::collection::vec(-100f32..100f32, 5..30),
        secondary in prop::collection::vec(-100f32..100f32, 5..30)
    ) {
        if primary.len() != secondary.len() && !primary.is_empty() {
            let fused = Embedder::fuse(&primary, &secondary, 0.5);
            assert_eq!(fused, primary);
        }
    }
}

// Property 15: Embedding cosine under extreme values (no panic)
proptest! {
    #[test]
    fn prop_cosine_extreme_values_no_panic(
        a in prop::collection::vec(any::<f32>(), 5..20),
        b in prop::collection::vec(any::<f32>(), 5..20)
    ) {
        if a.len() == b.len() && !a.is_empty() {
            // Should not panic even with NaN/Inf
            let _similarity = Embedder::cosine_similarity(&a, &b);
        }
    }
}
