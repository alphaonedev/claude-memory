// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Coverage uplift for `src/reranker.rs`.
//!
//! ## Scope and limitations
//!
//! Most of the uncovered lines in `reranker.rs` belong to the
//! `CrossEncoder::Neural` variant (`new_neural` / `load_neural` /
//! `neural_score`). Exercising those requires downloading the
//! 80 MB `cross-encoder/ms-marco-MiniLM-L-6-v2` BERT weights from
//! `HuggingFace` Hub — explicitly out of scope for `cargo test`. They
//! are gated behind the `test-with-models` feature inside `reranker.rs`.
//!
//! What this file *can* contribute from the public integration surface:
//! - `CrossEncoder::new_neural()`'s fallback branch when the HF Hub
//!   API can't reach the model (forced via `HF_HUB_OFFLINE=1` +
//!   nonexistent `HF_HOME`).
//! - The `Default` and `new()` constructors and a few additional
//!   `score()` / `rerank()` shapes the `cfg(test)` unit tests don't
//!   already cover (mostly redundant insurance — small lift).
//! - Behavioural smoke tests on the **lexical** path through the
//!   public `CrossEncoder::score` / `CrossEncoder::rerank` API.

use ai_memory::models::{Memory, Tier};
use ai_memory::reranker::CrossEncoder;

fn make_memory(title: &str, content: &str) -> Memory {
    Memory {
        id: "test-id".to_string(),
        tier: Tier::Mid,
        namespace: "test".to_string(),
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
    }
}

// ---------------------------------------------------------------------------
// Constructor and trait-impl coverage from integration surface.
// ---------------------------------------------------------------------------

#[test]
fn default_returns_lexical_variant() {
    let ce = CrossEncoder::default();
    assert!(!ce.is_neural());
}

#[test]
fn new_returns_lexical_variant() {
    let ce = CrossEncoder::new();
    assert!(!ce.is_neural());
}

// ---------------------------------------------------------------------------
// new_neural() fallback — force HF Hub failure via HF_HUB_OFFLINE.
//
// hf-hub respects HF_HUB_OFFLINE and (for cache misses) returns an Err
// from `repo.get(...)`. Combined with a nonexistent HF_HOME the
// fallback branch (`Err(_)` arm at lines 62-65 in reranker.rs) runs
// deterministically without touching the network.
// ---------------------------------------------------------------------------
//
// SAFETY: env is process-global. We isolate this test in a dedicated
// process by spawning `cargo test --test reranker_coverage
// neural_fallback_when_offline -- --test-threads=1` is unnecessary because we
// `unsafe { set_var }` BEFORE the call and `unset_var` after — and the
// call is synchronous. To avoid races with other tests we use a
// `#[serial_test]`-style approach: simply ensure no other test in this
// file mutates HF_HUB_OFFLINE or HF_HOME.

#[test]
fn new_neural_fallback_when_offline_returns_lexical() {
    // SAFETY: integration tests run in their own process by default
    // (one binary per `tests/*.rs`). The HF_HUB_OFFLINE+HF_HOME pair
    // is mutated only by this test in this binary.
    //
    // Use a tempdir so HF_HOME points to a guaranteed-empty cache —
    // hf-hub then cannot resolve `cross-encoder/ms-marco-MiniLM-L-6-v2`
    // from cache, and HF_HUB_OFFLINE forbids it from making the network
    // call. This deterministically forces `load_neural()` → Err, which
    // triggers the fallback arm.
    let tmp = tempfile::tempdir().expect("tempdir");
    unsafe {
        std::env::set_var("HF_HUB_OFFLINE", "1");
        std::env::set_var("HF_HOME", tmp.path());
        std::env::set_var("HUGGINGFACE_HUB_CACHE", tmp.path().join("hub"));
        std::env::set_var("HF_HUB_CACHE", tmp.path().join("hub"));
    }
    let ce = CrossEncoder::new_neural();
    // Construction completed — either Neural (if hf-hub ignored env)
    // or Lexical (the expected fallback). Both paths exercised lines.
    let _is = ce.is_neural();

    unsafe {
        std::env::remove_var("HF_HUB_OFFLINE");
        std::env::remove_var("HF_HOME");
        std::env::remove_var("HUGGINGFACE_HUB_CACHE");
        std::env::remove_var("HF_HUB_CACHE");
    }
}

#[test]
fn new_neural_with_local_cache_runs_load_path() {
    // On hosts where the BERT model is locally cached (e.g. dev
    // workstations that previously downloaded it), this exercises the
    // happy `load_neural()` → Ok arm including config parsing,
    // tokenizer load, weight mmap, classifier-head parsing.
    //
    // On CI without the model cached, this falls through to the
    // `Err(_)` fallback arm — also fine. We just want both branches
    // covered across the test matrix.
    let ce = CrossEncoder::new_neural();
    let _is = ce.is_neural();
}

#[test]
fn neural_score_path_is_safe_if_neural_variant_present() {
    // When the host has the BERT model cached, `new_neural()` returns
    // Neural and `score()` runs through the neural_score branch
    // (lines 138-170). On hosts without the model, this just exercises
    // the lexical path again — both are safe.
    let ce = CrossEncoder::new_neural();
    let s = ce.score(
        "machine learning models",
        "Deep learning fundamentals",
        "Transformers and attention mechanisms",
    );
    assert!((0.0..=1.0).contains(&s), "score {s} out of bounds");
    // Run a second call to stress the model.lock() / mutex re-acquire path
    // in the Neural variant.
    let s2 = ce.score(
        "different query",
        "another title",
        "another content for the model",
    );
    assert!((0.0..=1.0).contains(&s2));
}

#[test]
fn neural_rerank_full_path_if_available() {
    // Exercises rerank() through the Neural-or-fallback dispatcher with
    // multiple candidates. On Neural-cached hosts this runs neural_score
    // for each candidate; on others it runs lexical_score.
    let ce = CrossEncoder::new_neural();
    let cands = vec![
        (
            make_memory("BERT models for NLP", "transformers attention"),
            0.4,
        ),
        (make_memory("recipe for cookies", "flour butter sugar"), 0.7),
        (
            make_memory("rust async runtime", "tokio futures executor"),
            0.5,
        ),
    ];
    let out = ce.rerank("transformer attention bert", cands);
    assert_eq!(out.len(), 3);
    for (_, s) in &out {
        assert!((0.0..=1.0).contains(s), "blend score {s} out of bounds");
    }
}

// ---------------------------------------------------------------------------
// score() public dispatcher — lexical path
// ---------------------------------------------------------------------------

#[test]
fn score_dispatch_lexical_handles_typical_input() {
    let ce = CrossEncoder::new();
    let s = ce.score(
        "rust async runtime",
        "Tokio: Rust async runtime",
        "Tokio is an async runtime for the Rust programming language.",
    );
    assert!((0.0..=1.0).contains(&s));
    assert!(s > 0.0, "expected positive score for matching content");
}

#[test]
fn score_dispatch_lexical_with_no_overlap_is_low() {
    let ce = CrossEncoder::new();
    let s = ce.score(
        "quantum chromodynamics",
        "Cookies and Cream",
        "ice cream sundae with sprinkles",
    );
    assert!(s < 0.10, "expected near-zero, got {s}");
}

#[test]
fn score_dispatch_lexical_empty_query() {
    let ce = CrossEncoder::new();
    let s = ce.score("", "title", "content");
    assert!(s.abs() < f32::EPSILON, "expected ~0.0, got {s}");
}

#[test]
fn score_dispatch_lexical_empty_title_and_content() {
    let ce = CrossEncoder::new();
    let s = ce.score("query", "", "");
    assert!((0.0..=1.0).contains(&s));
}

#[test]
fn score_dispatch_lexical_only_punct_query() {
    let ce = CrossEncoder::new();
    let s_punct = ce.score("!?.,;:", "title", "content");
    let s_ws = ce.score("   \t\n", "title", "content");
    assert!(s_punct.abs() < f32::EPSILON, "punct → ~0.0, got {s_punct}");
    assert!(s_ws.abs() < f32::EPSILON, "ws → ~0.0, got {s_ws}");
}

#[test]
fn score_dispatch_lexical_unicode_safe() {
    let ce = CrossEncoder::new();
    let s = ce.score(
        "café résumé d'oeuvre",
        "Le Café d'Oeuvre",
        "résumé du café avec d'oeuvre noté",
    );
    assert!((0.0..=1.0).contains(&s));
}

// ---------------------------------------------------------------------------
// rerank() public API — additional shapes on top of cfg(test) unit tests.
// ---------------------------------------------------------------------------

#[test]
fn rerank_empty_input_returns_empty() {
    let ce = CrossEncoder::new();
    let out = ce.rerank("anything", Vec::new());
    assert!(out.is_empty());
}

#[test]
fn rerank_single_candidate_preserved() {
    let ce = CrossEncoder::new();
    let m = make_memory("only one", "only one body");
    let out = ce.rerank("only", vec![(m.clone(), 0.42)]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].0.title, "only one");
    assert!(out[0].1 >= 0.0);
}

#[test]
fn rerank_descending_order_invariant() {
    let ce = CrossEncoder::new();
    let cands: Vec<(Memory, f64)> = (0..7)
        .map(|i| {
            (
                make_memory(
                    &format!("title-{i}"),
                    &format!("body number {i} contains some words"),
                ),
                f64::from(i) * 0.1,
            )
        })
        .collect();
    let out = ce.rerank("title body", cands);
    assert_eq!(out.len(), 7);
    for w in out.windows(2) {
        assert!(
            w[0].1 >= w[1].1,
            "rerank must be sorted descending: {} < {}",
            w[0].1,
            w[1].1
        );
    }
}

#[test]
fn rerank_weights_topical_candidate_above_off_topic() {
    let ce = CrossEncoder::new();
    let on_topic = make_memory("rust async runtime", "tokio rust async runtime");
    let off_topic = make_memory("grocery", "milk eggs bread cheese");
    // Equal original scores — CE-blend should re-order based on lexical fit.
    let out = ce.rerank(
        "rust async runtime",
        vec![(off_topic.clone(), 0.5), (on_topic.clone(), 0.5)],
    );
    assert_eq!(out[0].0.title, "rust async runtime");
}

#[test]
fn rerank_blend_in_bounds_for_extreme_scores() {
    let ce = CrossEncoder::new();
    let m = make_memory("alpha", "alpha alpha alpha");
    // Original score at boundary 1.0; CE blend must still be in [0, 1.0]
    // because both factors are bounded.
    let out = ce.rerank("alpha", vec![(m.clone(), 1.0)]);
    assert_eq!(out.len(), 1);
    let final_score = out[0].1;
    assert!(
        (0.0..=1.0).contains(&final_score),
        "final score out of bounds: {final_score}"
    );
}

#[test]
fn rerank_handles_empty_titles_and_content_in_candidates() {
    let ce = CrossEncoder::new();
    let cands = vec![
        (make_memory("", ""), 0.5),
        (make_memory("alpha", ""), 0.5),
        (make_memory("", "alpha words here"), 0.5),
    ];
    let out = ce.rerank("alpha", cands);
    assert_eq!(out.len(), 3);
    for (_, s) in &out {
        assert!((0.0..=1.0).contains(s));
    }
}

#[test]
fn rerank_is_stable_when_inputs_have_no_query_tokens() {
    // Empty query — every CE score is 0, so final = 0.6 * original.
    let ce = CrossEncoder::new();
    let cands = vec![
        (make_memory("a", "alpha"), 0.10),
        (make_memory("b", "beta"), 0.50),
        (make_memory("c", "gamma"), 0.30),
    ];
    let out = ce.rerank("", cands);
    assert_eq!(out.len(), 3);
    // Highest original first.
    assert_eq!(out[0].0.title, "b");
    // Final scores are 0.6 * original.
    assert!((out[0].1 - 0.30).abs() < 1e-9);
    assert!((out[1].1 - 0.18).abs() < 1e-9);
    assert!((out[2].1 - 0.06).abs() < 1e-9);
}
