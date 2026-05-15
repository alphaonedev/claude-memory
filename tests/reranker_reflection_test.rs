// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 L2-8 — reflection-aware reranker boost.
//!
//! Pinned acceptance criteria (issue #673):
//!
//! 1. Query "what patterns" on 10 observations + 2 reflections — both
//!    reflections rank in the top-5 with the boost on, but would rank
//!    lower without the boost.
//! 2. A depth-2 reflection outscores a depth-1 reflection of equal base
//!    relevance (per-depth multiplier monotone in depth, up to the cap).
//! 3. `boost = 1.0` reproduces the pre-L2-8 ordering exactly — the
//!    boost-disabled path is a pure no-op the regression suite can pin.
//!
//! These tests use the public reranker API only (no daemon, no DB)
//! because the boost is applied AFTER the cross-encoder rerank — the
//! recall pipeline shape is incidental to the contract.

use ai_memory::models::{Memory, MemoryKind, Tier};
use ai_memory::reranker::{CrossEncoder, ReflectionBoostConfig};

/// Build a memory fixture. `depth = 0` and `kind = Observation` mirror
/// the v0.7.0 schema defaults; tests override the two fields they care
/// about.
fn mk(id: &str, title: &str, content: &str, kind: MemoryKind, reflection_depth: i32) -> Memory {
    Memory {
        id: id.to_string(),
        tier: Tier::Mid,
        namespace: "ns".to_string(),
        title: title.to_string(),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({}),
        reflection_depth,
        memory_kind: kind,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
    }
}

// ---------------------------------------------------------------------------
// Defaults — pin the documented constants so the spec contract holds.
// ---------------------------------------------------------------------------

#[test]
fn default_boost_factor_is_1_2() {
    let cfg = ReflectionBoostConfig::default();
    assert!(
        (cfg.boost - 1.2).abs() < f32::EPSILON,
        "default boost must be 1.2, got {}",
        cfg.boost
    );
    assert!(
        (cfg.per_depth_increment - 0.05).abs() < f32::EPSILON,
        "default per_depth_increment must be 0.05, got {}",
        cfg.per_depth_increment
    );
    assert_eq!(
        cfg.max_depth_cap, 3,
        "default max_depth_cap must mirror governance default (3)"
    );
}

#[test]
fn disabled_config_is_strict_no_op() {
    let cfg = ReflectionBoostConfig::disabled();
    assert!((cfg.boost - 1.0).abs() < f32::EPSILON);
    // For ANY memory (Observation or Reflection at any depth) the
    // multiplier must equal 1.0.
    let obs = mk("o", "t", "c", MemoryKind::Observation, 0);
    let refl0 = mk("r0", "t", "c", MemoryKind::Reflection, 0);
    let refl_deep = mk("rD", "t", "c", MemoryKind::Reflection, 10);
    assert!((cfg.factor_for(&obs) - 1.0).abs() < 1e-9);
    assert!((cfg.factor_for(&refl0) - 1.0).abs() < 1e-9);
    assert!((cfg.factor_for(&refl_deep) - 1.0).abs() < 1e-9);
}

#[test]
fn factor_for_observation_is_always_one() {
    let cfg = ReflectionBoostConfig::default();
    let obs_d0 = mk("o0", "t", "c", MemoryKind::Observation, 0);
    let obs_d5 = mk("o5", "t", "c", MemoryKind::Observation, 5);
    // Observations ignore reflection_depth entirely.
    assert!((cfg.factor_for(&obs_d0) - 1.0).abs() < 1e-9);
    assert!((cfg.factor_for(&obs_d5) - 1.0).abs() < 1e-9);
}

#[test]
fn factor_for_reflection_grows_with_depth_up_to_cap() {
    let cfg = ReflectionBoostConfig::default();
    let r0 = mk("r0", "t", "c", MemoryKind::Reflection, 0);
    let r1 = mk("r1", "t", "c", MemoryKind::Reflection, 1);
    let r2 = mk("r2", "t", "c", MemoryKind::Reflection, 2);
    let r3 = mk("r3", "t", "c", MemoryKind::Reflection, 3);
    let r4 = mk("r4", "t", "c", MemoryKind::Reflection, 4); // past cap
    let r99 = mk("r99", "t", "c", MemoryKind::Reflection, 99); // far past cap

    // Boost = 1.2; per_depth_increment = 0.05; cap = 3.
    // factor(depth=k) = 1.2 * (1.0 + 0.05 * min(k, 3)).
    //
    // The runtime computes this in f64 starting from the f32 config
    // fields (boost, per_depth_increment) — match that conversion in
    // the expected value so we compare apples to apples.
    let boost_f64 = f64::from(1.2_f32);
    let inc_f64 = f64::from(0.05_f32);
    let f = |k: u32| boost_f64 * inc_f64.mul_add(f64::from(k), 1.0);
    let eq = |a: f64, b: f64| (a - b).abs() < 1e-9;

    assert!(eq(cfg.factor_for(&r0), f(0)));
    assert!(eq(cfg.factor_for(&r1), f(1)));
    assert!(eq(cfg.factor_for(&r2), f(2)));
    assert!(eq(cfg.factor_for(&r3), f(3)));
    // Past the cap, factor is clamped to factor(cap).
    assert!(eq(cfg.factor_for(&r4), f(3)));
    assert!(eq(cfg.factor_for(&r99), f(3)));
}

#[test]
fn factor_for_negative_depth_treated_as_zero() {
    // Defensive: SQL i32 column accepts -1 if a bad write upstream
    // sneaks through. The reranker must NOT produce a negative
    // multiplier in that case — it clamps to depth=0.
    let cfg = ReflectionBoostConfig::default();
    let r_neg = mk("rn", "t", "c", MemoryKind::Reflection, -5);
    let r_zero = mk("rz", "t", "c", MemoryKind::Reflection, 0);
    assert!((cfg.factor_for(&r_neg) - cfg.factor_for(&r_zero)).abs() < 1e-9);
}

// ---------------------------------------------------------------------------
// Acceptance #1 — 10 obs + 2 reflections, boost on lifts reflections
// into top-5 for an abstraction-shaped query.
// ---------------------------------------------------------------------------

#[test]
fn ten_obs_two_reflections_boost_lifts_reflections_top5() {
    let ce = CrossEncoder::new();
    // Abstraction-shaped query: "what patterns".
    let query = "what patterns";

    // 10 observations with a tightly-clustered original score band so
    // the pre-boost top-5 is observations only (reflections sit just
    // below the cliff). Each observation has the SAME content+title so
    // the cross-encoder score is identical across them — original score
    // is the only differentiator pre-boost. The "patterns" token gives
    // them a non-trivial but uniform CE score on the abstraction query.
    let mut cands: Vec<(Memory, f64)> = (0..10)
        .map(|i| {
            let mem = mk(
                &format!("obs-{i}"),
                "patterns in logs",
                "we noticed patterns in log output today",
                MemoryKind::Observation,
                0,
            );
            // Linear descending band 0.80..0.71 — pre-boost top-5 is
            // obs-0..obs-4. The reflections at 0.70 sit just below the
            // cliff so a 1.2× boost is enough to flip them in.
            let score = 0.80 - 0.01 * f64::from(i);
            (mem, score)
        })
        .collect();

    // 2 reflections with matching surface so CE score is comparable to
    // the observations, but with an original score that's a hair below
    // the observation band. Pre-boost: not in top-5. With the default
    // 1.2× boost (+ depth bump), they lift above obs-3/obs-4.
    cands.push((
        mk(
            "refl-1",
            "patterns in logs",
            "we noticed patterns in log output today",
            MemoryKind::Reflection,
            1,
        ),
        0.70,
    ));
    cands.push((
        mk(
            "refl-2",
            "patterns in logs",
            "we noticed patterns in log output today",
            MemoryKind::Reflection,
            2,
        ),
        0.70,
    ));

    // --- Pre-boost (legacy rerank) — reflections should NOT be top-5 ---
    let no_boost =
        ce.rerank_with_reflection_boost(query, cands.clone(), &ReflectionBoostConfig::disabled());
    let top5_no_boost: Vec<&str> = no_boost
        .iter()
        .take(5)
        .map(|(m, _)| m.id.as_str())
        .collect();
    let no_boost_has_refl = top5_no_boost.iter().any(|id| id.starts_with("refl-"));
    assert!(
        !no_boost_has_refl,
        "expected no reflections in pre-boost top-5, got {top5_no_boost:?}"
    );

    // --- With default boost (1.2) — both reflections lift into top-5 ---
    let boosted = ce.rerank_with_reflection_boost(query, cands, &ReflectionBoostConfig::default());
    let top5_boosted: Vec<&str> = boosted.iter().take(5).map(|(m, _)| m.id.as_str()).collect();
    let refl_count = top5_boosted
        .iter()
        .filter(|id| id.starts_with("refl-"))
        .count();
    assert_eq!(
        refl_count, 2,
        "expected both reflections in boosted top-5, got {top5_boosted:?}"
    );
}

// ---------------------------------------------------------------------------
// Acceptance #2 — equal-base-relevance, depth-2 outranks depth-1.
// ---------------------------------------------------------------------------

#[test]
fn equal_base_relevance_depth2_outranks_depth1() {
    let ce = CrossEncoder::new();
    // Identical title/content so the cross-encoder score is the SAME for
    // both — the per-depth multiplier is the only differentiator.
    let r1 = mk(
        "r1",
        "shared abstraction",
        "summary of common observations",
        MemoryKind::Reflection,
        1,
    );
    let r2 = mk(
        "r2",
        "shared abstraction",
        "summary of common observations",
        MemoryKind::Reflection,
        2,
    );
    // Identical original score so the only delta is the depth-bump.
    let cands = vec![(r1.clone(), 0.5), (r2.clone(), 0.5)];

    let out = ce.rerank_with_reflection_boost(
        "shared abstraction",
        cands,
        &ReflectionBoostConfig::default(),
    );
    assert_eq!(out.len(), 2);
    // Depth-2 must lead.
    assert_eq!(out[0].0.id, "r2", "depth-2 must outrank depth-1");
    // And its score must be strictly greater.
    assert!(
        out[0].1 > out[1].1,
        "depth-2 final ({}) must be strictly > depth-1 final ({})",
        out[0].1,
        out[1].1
    );
}

#[test]
fn reflection_at_cap_outranks_observation_of_equal_base() {
    // A depth-3 reflection (at the cap) should outrank an observation of
    // identical base relevance because the boost multiplier is > 1.
    let ce = CrossEncoder::new();
    let obs = mk(
        "obs",
        "common phrase here",
        "neutral content for both",
        MemoryKind::Observation,
        0,
    );
    let refl = mk(
        "refl",
        "common phrase here",
        "neutral content for both",
        MemoryKind::Reflection,
        3,
    );
    let cands = vec![(obs, 0.5), (refl, 0.5)];
    let out =
        ce.rerank_with_reflection_boost("common phrase", cands, &ReflectionBoostConfig::default());
    assert_eq!(out[0].0.id, "refl");
}

// ---------------------------------------------------------------------------
// Acceptance #3 — boost=1.0 regression pin: identical to pre-L2-8 rerank.
// ---------------------------------------------------------------------------

#[test]
fn boost_disabled_matches_legacy_rerank_byte_identical() {
    let ce = CrossEncoder::new();
    // Mix of reflections and observations at various depths — the
    // disabled config must NOT touch any of them.
    let cands = vec![
        (
            mk(
                "o-a",
                "alpha words",
                "alpha body",
                MemoryKind::Observation,
                0,
            ),
            0.42,
        ),
        (
            mk("o-b", "beta words", "beta body", MemoryKind::Observation, 0),
            0.31,
        ),
        (
            mk(
                "r-1",
                "alpha summary",
                "summarized alpha",
                MemoryKind::Reflection,
                1,
            ),
            0.50,
        ),
        (
            mk(
                "r-2",
                "beta summary",
                "summarized beta",
                MemoryKind::Reflection,
                2,
            ),
            0.45,
        ),
        (
            mk(
                "r-3",
                "gamma summary",
                "summarized gamma",
                MemoryKind::Reflection,
                3,
            ),
            0.60,
        ),
    ];

    let legacy = ce.rerank("alpha", cands.clone());
    let disabled =
        ce.rerank_with_reflection_boost("alpha", cands, &ReflectionBoostConfig::disabled());

    // Same length.
    assert_eq!(legacy.len(), disabled.len());
    // Same ids in the same order.
    let legacy_ids: Vec<&str> = legacy.iter().map(|(m, _)| m.id.as_str()).collect();
    let disabled_ids: Vec<&str> = disabled.iter().map(|(m, _)| m.id.as_str()).collect();
    assert_eq!(legacy_ids, disabled_ids);
    // And byte-identical f64 scores (no boost applied → blend formula
    // is the only thing computing them, in both paths).
    for (a, b) in legacy.iter().zip(disabled.iter()) {
        assert!(
            (a.1 - b.1).abs() < f64::EPSILON,
            "score diverged for {}: legacy={}, disabled={}",
            a.0.id,
            a.1,
            b.1
        );
    }
}

// ---------------------------------------------------------------------------
// Batched parity — boost is applied in `rerank_batch_with_reflection_boost`
// the same way `rerank_with_reflection_boost` applies it.
// ---------------------------------------------------------------------------

#[test]
fn batched_with_boost_matches_per_query_with_boost() {
    let ce = CrossEncoder::new();
    let q1 = "alpha summary";
    let q2 = "beta observation";
    let cands_q1 = vec![
        (
            mk(
                "a-obs",
                "alpha alpha",
                "alpha body",
                MemoryKind::Observation,
                0,
            ),
            0.45,
        ),
        (
            mk(
                "a-refl",
                "alpha summary",
                "alpha alpha summary",
                MemoryKind::Reflection,
                2,
            ),
            0.40,
        ),
    ];
    let cands_q2 = vec![
        (
            mk(
                "b-obs",
                "beta beta",
                "beta observation body",
                MemoryKind::Observation,
                0,
            ),
            0.55,
        ),
        (
            mk(
                "b-refl",
                "beta abstract",
                "summary of beta observations",
                MemoryKind::Reflection,
                1,
            ),
            0.55,
        ),
    ];

    let cfg = ReflectionBoostConfig::default();

    // Batched result.
    let batched = ce.rerank_batch_with_reflection_boost(
        vec![
            (q1.to_string(), cands_q1.clone()),
            (q2.to_string(), cands_q2.clone()),
        ],
        &cfg,
    );

    // Per-query result.
    let per_q1 = ce.rerank_with_reflection_boost(q1, cands_q1, &cfg);
    let per_q2 = ce.rerank_with_reflection_boost(q2, cands_q2, &cfg);

    assert_eq!(batched.len(), 2);
    // Same ordering (and same scores) — the batched path is a pure
    // throughput optimisation, not a behavioural fork.
    for (a, b) in batched[0].iter().zip(per_q1.iter()) {
        assert_eq!(a.0.id, b.0.id);
        assert!((a.1 - b.1).abs() < f64::EPSILON);
    }
    for (a, b) in batched[1].iter().zip(per_q2.iter()) {
        assert_eq!(a.0.id, b.0.id);
        assert!((a.1 - b.1).abs() < f64::EPSILON);
    }
}

#[test]
fn batched_legacy_rerank_batch_matches_disabled_boost() {
    // The bare `rerank_batch` delegates to the boost-aware path with
    // `disabled()` — pin that the two are byte-identical.
    let ce = CrossEncoder::new();
    let cands_q = vec![
        (mk("a", "alpha", "alpha", MemoryKind::Observation, 0), 0.5),
        (
            mk("r", "alpha summary", "alpha", MemoryKind::Reflection, 2),
            0.5,
        ),
    ];
    let legacy = ce.rerank_batch(vec![("alpha".to_string(), cands_q.clone())]);
    let disabled = ce.rerank_batch_with_reflection_boost(
        vec![("alpha".to_string(), cands_q)],
        &ReflectionBoostConfig::disabled(),
    );
    assert_eq!(legacy.len(), disabled.len());
    assert_eq!(legacy[0].len(), disabled[0].len());
    for (a, b) in legacy[0].iter().zip(disabled[0].iter()) {
        assert_eq!(a.0.id, b.0.id);
        assert!((a.1 - b.1).abs() < f64::EPSILON);
    }
}

// ---------------------------------------------------------------------------
// BatchedReranker — daemon-shape: confirm default config and the
// configurable constructor surface.
// ---------------------------------------------------------------------------

#[test]
fn batched_reranker_default_boost_is_1_2() {
    use ai_memory::reranker::BatchedReranker;
    let br = BatchedReranker::new(CrossEncoder::new());
    let cfg = br.reflection_boost();
    assert!((cfg.boost - 1.2).abs() < f32::EPSILON);
}

#[test]
fn batched_reranker_with_disabled_boost_matches_legacy() {
    use ai_memory::reranker::BatchedReranker;
    let cands = vec![
        (mk("a", "alpha", "alpha", MemoryKind::Observation, 0), 0.5),
        (
            mk("r", "alpha summary", "alpha", MemoryKind::Reflection, 2),
            0.5,
        ),
    ];
    // Direct legacy rerank.
    let legacy = CrossEncoder::new().rerank("alpha", cands.clone());
    // Through BatchedReranker with the boost explicitly disabled.
    let br = BatchedReranker::with_reflection_boost(
        CrossEncoder::new(),
        ReflectionBoostConfig::disabled(),
    );
    let via_batched = br.rerank("alpha", cands);
    assert_eq!(legacy.len(), via_batched.len());
    for (a, b) in legacy.iter().zip(via_batched.iter()) {
        assert_eq!(a.0.id, b.0.id);
        assert!((a.1 - b.1).abs() < f64::EPSILON);
    }
}
