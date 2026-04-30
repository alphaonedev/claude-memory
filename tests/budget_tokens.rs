// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// Phase P6 (R1) — `budget_tokens` recall acceptance tests.
//
// These four tests are the contract for the v0.6.3.1 R1 "budget_tokens
// recall" feature recovery. They exercise `db::apply_token_budget`
// directly so the suite stays fast (no embedder, no FTS roundtrip) and
// pins the deterministic cl100k_base tokenizer-driven greedy fill.
//
// The matching CLI / HTTP / MCP surface tests live in
// `tests/integration.rs` (under "Task 1.11 / Phase P6 — Context-Budget-
// Aware Recall") and `src/mcp.rs` (`handle_recall_*` mod-tests). Keeping
// the unit-level tests here lets us run `AI_MEMORY_NO_CONFIG=1
// cargo test --test budget_tokens` as a tight feedback loop while
// iterating on the budget logic itself.

use ai_memory::db::{BudgetOutcome, apply_token_budget, count_tokens_cl100k};
use ai_memory::models::{Memory, Tier};
use serde_json::json;

fn mem_with_content(id: &str, content: &str) -> Memory {
    Memory {
        id: id.to_string(),
        tier: Tier::Long,
        namespace: "test".to_string(),
        title: format!("title-{id}"),
        content: content.to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: "2026-04-29T00:00:00Z".to_string(),
        updated_at: "2026-04-29T00:00:00Z".to_string(),
        last_accessed_at: None,
        expires_at: None,
        metadata: json!({}),
    }
}

/// Score is irrelevant to budget logic — descend monotonically so the
/// "rank order preserved" assertions hit on the obvious ordering.
fn ranked(items: &[(&str, &str)]) -> Vec<(Memory, f64)> {
    items
        .iter()
        .enumerate()
        .map(|(i, (id, content))| {
            let score = 1.0 - (i as f64) * 0.01;
            (mem_with_content(id, content), score)
        })
        .collect()
}

#[test]
fn budget_tokens_returns_subset_under_budget() {
    // R1 acceptance #1 — budget=10 returns ≤2 short memories. With
    // cl100k_base, "hi" is 1 token, "hello world" is 2 tokens, so
    // `["hi", "hello world", "hi", "hi", ...]` should fit ~10 of these
    // tiny items in a 10-token budget. Use longer content to exercise
    // the "stops at first overflow" path with a small subset.
    let scored = ranked(&[
        ("a", "alpha beta gamma delta"),  // ~4-5 tok
        ("b", "epsilon zeta eta theta"),  // ~4-5 tok
        ("c", "iota kappa lambda mu nu"), // ~5-6 tok — should not fit
        ("d", "more more more more"),     // dropped
    ]);
    let (out, outcome) = apply_token_budget(scored.clone(), Some(10));

    // Always at least one, always ≤ candidates.
    assert!(
        !out.is_empty(),
        "subset must be non-empty for matched input"
    );
    assert!(
        out.len() < scored.len(),
        "subset must be smaller than input"
    );

    // Tokens used must be the cl100k_base sum of the returned content.
    let recomputed: usize = out
        .iter()
        .map(|(m, _)| count_tokens_cl100k(&m.content))
        .sum();
    assert_eq!(outcome.tokens_used, recomputed);

    // Either the budget was respected, or overflow=true (top item alone
    // exceeded). With 4-5 token contents this branch should never hit.
    if !outcome.budget_overflow {
        assert!(
            outcome.tokens_used <= 10,
            "tokens_used ({}) must be <= budget (10) when not overflowing",
            outcome.tokens_used
        );
    }

    // Rank order preserved — first returned id matches first input id.
    assert_eq!(out[0].0.id, "a");

    // Dropped count tallies.
    assert_eq!(outcome.memories_dropped, scored.len() - out.len());
    assert_eq!(
        outcome.tokens_remaining,
        Some(10usize.saturating_sub(outcome.tokens_used))
    );
}

#[test]
fn budget_tokens_returns_one_memory_when_overflow() {
    // R1 acceptance #2 — when the highest-ranked memory alone exceeds
    // the budget, return it anyway with meta.budget_overflow=true. This
    // is the R1 always-return-at-least-one guarantee (no successful
    // recall ever returns zero rows when matches exist, even under a
    // pathologically tight budget).
    let big_content = "lorem ipsum ".repeat(50); // ~100+ cl100k tokens
    let scored = ranked(&[("only", &big_content)]);
    let big_tokens = count_tokens_cl100k(&big_content);
    assert!(
        big_tokens > 1,
        "fixture sanity: big content must exceed budget=1"
    );

    let (out, outcome) = apply_token_budget(scored, Some(1));
    assert_eq!(out.len(), 1, "R1: at least one memory always returned");
    assert_eq!(out[0].0.id, "only");
    assert!(outcome.budget_overflow, "overflow flag must be set");
    assert_eq!(outcome.tokens_used, big_tokens);
    assert_eq!(outcome.tokens_remaining, Some(0));
    assert_eq!(outcome.memories_dropped, 0);
}

#[test]
fn budget_tokens_zero_returns_zero_memories() {
    // R1 acceptance #3 — budget_tokens=0 ("give me nothing") returns
    // zero memories with overflow=false. Distinct from a tight-but-
    // non-zero budget which returns at least one memory under the R1
    // always-return-at-least-one guarantee.
    let scored = ranked(&[("a", "would have fit a larger budget"), ("b", "ditto")]);
    let (out, outcome) = apply_token_budget(scored.clone(), Some(0));
    assert!(out.is_empty(), "budget=0 returns zero memories");
    assert_eq!(outcome.tokens_used, 0);
    assert_eq!(outcome.tokens_remaining, Some(0));
    assert_eq!(outcome.memories_dropped, scored.len());
    assert!(
        !outcome.budget_overflow,
        "overflow=false because the user asked for nothing"
    );
}

#[test]
fn budget_tokens_unset_preserves_v063_behavior_byte_for_byte() {
    // R1 acceptance #4 — `budget_tokens=None` is the v0.6.3 baseline.
    // Every candidate is returned, in input order, with `tokens_used`
    // populated as a fast byte-heuristic tally (`content.len() / 4`)
    // for callers that want to observe the cost without enforcing it.
    // `tokens_remaining` is None (no budget), `memories_dropped` is 0,
    // `budget_overflow` is false. The cl100k_base BPE encoder is NOT
    // run on this path — running it on every recall regardless of
    // caller intent would impose a ~200 ms cold-start (BPE table
    // parse) that the bench harness's recall_hot p95 budget cannot
    // absorb. When the caller supplies a budget they're opting into
    // the precise count.
    let scored = ranked(&[
        ("a", "alpha beta gamma"),
        ("b", "delta epsilon zeta"),
        ("c", "eta theta iota"),
    ]);
    let original = scored.clone();
    let (out, outcome) = apply_token_budget(scored, None);

    assert_eq!(
        out.len(),
        original.len(),
        "unset budget returns every candidate"
    );
    for (returned, expected) in out.iter().zip(original.iter()) {
        assert_eq!(returned.0.id, expected.0.id, "rank order preserved");
        // Score is f64; compare to within an exact bit pattern since we
        // never modify the score in the budget pass.
        assert_eq!(returned.1.to_bits(), expected.1.to_bits());
    }

    // Byte heuristic — same shape as Task 1.11's pre-P6 contract.
    let expected_tokens: usize = original.iter().map(|(m, _)| m.content.len() / 4).sum();
    assert_eq!(outcome.tokens_used, expected_tokens, "byte-heuristic tally");
    assert_eq!(outcome.tokens_remaining, None, "no budget => no remaining");
    assert_eq!(outcome.memories_dropped, 0);
    assert!(!outcome.budget_overflow);
}

// ---------------------------------------------------------------------------
// Determinism — cl100k_base must produce identical counts across calls.
// ---------------------------------------------------------------------------

#[test]
fn cl100k_tokenizer_is_deterministic() {
    // Defensive — the BPE table is bundled in the crate so the count
    // must be stable across calls. If this ever drifts (e.g. a future
    // tiktoken-rs upgrade swaps tables) the budget contract breaks
    // silently; this test catches it loudly.
    let s = "The quick brown fox jumps over the lazy dog.";
    let a = count_tokens_cl100k(s);
    let b = count_tokens_cl100k(s);
    let c = count_tokens_cl100k(s);
    assert_eq!(a, b);
    assert_eq!(b, c);
    // Sanity: well above the byte-heuristic floor and well below the
    // character count. (The exact value for cl100k on this string is 10
    // — pin it so a future-tiktoken regression jumps out in CI logs.)
    assert_eq!(a, 10, "cl100k_base count for the canonical pangram");
}

#[test]
fn budget_outcome_round_trips_meta_fields() {
    // Smoke test — every public field on BudgetOutcome is populated as
    // expected for a happy-path subset. Catches struct-field renames
    // that would otherwise only surface at the JSON surface layer.
    let scored = ranked(&[
        ("a", "alpha"),
        ("b", "beta gamma"),
        ("c", "delta epsilon zeta eta theta iota"), // largest, won't fit
    ]);
    let (out, outcome): (Vec<_>, BudgetOutcome) = apply_token_budget(scored.clone(), Some(5));

    assert!(out.len() >= 1);
    assert!(outcome.tokens_used > 0);
    assert!(outcome.tokens_remaining.is_some());
    assert!(outcome.memories_dropped <= scored.len());
}
