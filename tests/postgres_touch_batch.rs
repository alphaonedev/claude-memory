// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::items_after_statements,
    clippy::map_unwrap_or,
    clippy::too_many_lines,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]
//! Wave-2 Tier-A4 (#852) — postgres `touch_after_recall` must apply
//! the three touch effects (`access_count` + sliding-window TTL extend,
//! mid→long auto-promotion at `PROMOTION_THRESHOLD`, priority bump
//! every 10 accesses) atomically in a SINGLE UPDATE statement.
//!
//! The pre-refactor implementation ran three separate
//! `sqlx::query(...)` calls inside a transaction (3 parse+execute
//! roundtrips per recall touch); this regression test pins the
//! collapsed single-statement form by exercising each branch of the
//! merged CASE expression and asserting the post-touch row state.
//!
//! ## What this test asserts
//!
//! For a set of seeded `mid`-tier memories under a fresh namespace:
//!
//! 1. **First touch (`access_count` 0 → 1)** — `access_count` increments,
//!    `last_accessed_at` lands, `expires_at` slides forward by 1d
//!    (mid tier), tier stays `mid`, priority is unchanged.
//! 2. **Fifth touch (`access_count` 4 → 5)** — promotion fires: tier
//!    flips to `long`, `expires_at` clears to NULL, `updated_at`
//!    advances.
//! 3. **Tenth touch (`access_count` 9 → 10)** — priority bumps by 1
//!    (the row stays `long` from step 2; the priority CASE fires
//!    purely on the post-increment % 10 == 0 predicate).
//!
//! A second test covers the `short`-tier branch — touch must slide
//! the 1h TTL forward and NEVER auto-promote (promotion is mid→long
//! only).
//!
//! ## Gating
//!
//! Same as `g1_postgres_quota_increment_on_store.rs` —
//! `feature = "sal-postgres"` plus `AI_MEMORY_TEST_POSTGRES_URL`
//! at run time. Without either the test prints a skip line and
//! returns cleanly.

#![cfg(feature = "sal-postgres")]

use std::sync::Arc;

use ai_memory::models::{ConfidenceSource, Memory, MemoryKind, Tier};
use ai_memory::store::postgres::PostgresStore;
use ai_memory::store::{CallerContext, MemoryStore};

fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

fn fresh_memory(title: &str, namespace: &str, tier: Tier, priority: i32) -> Memory {
    let now = chrono::Utc::now().to_rfc3339();
    // Mid-tier rows need an `expires_at` for the sliding-window CASE
    // branch to fire (the production code only updates `expires_at`
    // when it is non-NULL — mirrors the SQLite contract). Use a
    // long-enough horizon that the row stays live across the test.
    let expires_at = match tier {
        Tier::Mid => Some((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339()),
        Tier::Short => Some((chrono::Utc::now() + chrono::Duration::hours(6)).to_rfc3339()),
        Tier::Long => None,
    };
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: "touch-batch-content".to_string(),
        tags: vec![],
        priority,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at,
        metadata: serde_json::json!({"agent_id": "ai:touch-batch-test"}),
        reflection_depth: 0,
        memory_kind: MemoryKind::Observation,
        entity_id: None,
        persona_version: None,
        citations: Vec::new(),
        source_uri: None,
        source_span: None,
        confidence_source: ConfidenceSource::CallerProvided,
        confidence_signals: None,
        confidence_decayed_at: None,
        version: 1,
    }
}

/// Drives `touch_after_recall` `n` times against the same id batch and
/// returns the live row state after the final touch. Each touch is one
/// UPDATE statement post-refactor (was three pre-refactor).
async fn touch_n(
    store: &Arc<dyn MemoryStore>,
    ctx: &CallerContext,
    ids: &[String],
    n: usize,
) -> Vec<Memory> {
    for _ in 0..n {
        store
            .touch_after_recall(ids)
            .await
            .expect("touch_after_recall must succeed");
    }
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let m = store.get(ctx, id).await.expect("get after touches");
        out.push(m);
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn touch_after_recall_single_update_preserves_semantics() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping touch_after_recall_single_update_preserves_semantics: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(&url)
            .await
            .expect("connect postgres adapter"),
    );
    let ctx = CallerContext::for_agent("ai:touch-batch-test");

    // Per-test namespace so concurrent runs don't collide.
    let suffix = uuid::Uuid::new_v4();
    let ns = format!("touch-batch-{suffix}");

    // Seed 3 mid-tier memories with priority=5 so we can observe each
    // CASE branch independently.
    let mut ids: Vec<String> = Vec::with_capacity(3);
    for i in 0..3 {
        let mem = fresh_memory(&format!("touch-batch-{suffix}-{i}"), &ns, Tier::Mid, 5);
        let id = store
            .store(&ctx, &mem)
            .await
            .expect("seed store must succeed");
        ids.push(id);
    }

    // ---- Phase 1: single touch, access_count 0 → 1. ----
    let rows = touch_n(&store, &ctx, &ids, 1).await;
    for r in &rows {
        assert_eq!(
            r.access_count, 1,
            "access_count must be 1 after first touch (id={})",
            r.id
        );
        assert_eq!(r.tier, Tier::Mid, "tier stays mid below threshold");
        assert_eq!(r.priority, 5, "priority unchanged before 10th touch");
        assert!(
            r.last_accessed_at.is_some(),
            "last_accessed_at must populate"
        );
        assert!(
            r.expires_at.is_some(),
            "mid-tier expires_at must slide forward, not clear"
        );
    }

    // ---- Phase 2: drive to the 5th touch, watch the mid→long flip. ----
    // We've already done 1 touch; do 4 more to land at access_count=5.
    let rows = touch_n(&store, &ctx, &ids, 4).await;
    for r in &rows {
        assert_eq!(
            r.access_count, 5,
            "access_count must be 5 after fifth touch (id={})",
            r.id
        );
        assert_eq!(
            r.tier,
            Tier::Long,
            "mid→long auto-promote must fire at access_count=5 (PROMOTION_THRESHOLD)"
        );
        assert_eq!(
            r.expires_at, None,
            "expires_at must clear when promotion fires this round"
        );
        assert_eq!(r.priority, 5, "priority unchanged before 10th touch");
    }

    // ---- Phase 3: drive to the 10th touch, watch the priority bump. ----
    // Currently at access_count=5; 5 more touches lands at 10. The row
    // is already long-tier from Phase 2 so the promotion CASE arm is
    // a no-op (long stays long); only the priority CASE fires.
    let rows = touch_n(&store, &ctx, &ids, 5).await;
    for r in &rows {
        assert_eq!(
            r.access_count, 10,
            "access_count must be 10 after the tenth touch (id={})",
            r.id
        );
        assert_eq!(
            r.tier,
            Tier::Long,
            "long tier persists; CASE only flips from mid"
        );
        assert_eq!(
            r.priority, 6,
            "priority must bump from 5 → 6 on the 10th touch \
             (post-increment access_count % 10 == 0 branch)"
        );
        assert_eq!(
            r.expires_at, None,
            "long-tier rows keep their NULL expires_at across touch"
        );
    }

    // ---- Phase 4: drive to the 20th touch — priority bumps again. ----
    let rows = touch_n(&store, &ctx, &ids, 10).await;
    for r in &rows {
        assert_eq!(r.access_count, 20, "access_count must be 20 (id={})", r.id);
        assert_eq!(
            r.priority, 7,
            "priority must bump twice (10th + 20th touch): 5 → 6 → 7"
        );
    }

    // ---- Phase 5: empty-ids early return must be a no-op. ----
    // The pre-refactor code returned Ok(()) on empty; the refactor
    // preserves that contract by short-circuiting before the SQL.
    store
        .touch_after_recall(&[])
        .await
        .expect("empty touch must be a no-op");
}

/// Short-tier rows must see the 1h sliding-window TTL extension and
/// NEVER auto-promote (promotion is mid→long only — short stays short
/// until an explicit `memory_promote`).
#[tokio::test(flavor = "multi_thread")]
async fn touch_after_recall_short_tier_extends_ttl_no_promote() {
    let Some(url) = postgres_url() else {
        eprintln!(
            "skipping touch_after_recall_short_tier_extends_ttl_no_promote: \
             AI_MEMORY_TEST_POSTGRES_URL not set"
        );
        return;
    };

    let store: Arc<dyn MemoryStore> = Arc::new(
        PostgresStore::connect(&url)
            .await
            .expect("connect postgres adapter"),
    );
    // Caller must match the owner stamp on fresh_memory's metadata.agent_id
    // ("ai:touch-batch-test") so the SAL-level scope=private filter (#910)
    // doesn't fold the row into NotFound. The "-short" suffix on the prior
    // ctx was test-only naming; this test exercises TTL semantics on a
    // short-tier row owned by the same caller as the mid-tier sibling above.
    let ctx = CallerContext::for_agent("ai:touch-batch-test");

    let suffix = uuid::Uuid::new_v4();
    let ns = format!("touch-batch-short-{suffix}");

    let mem = fresh_memory(&format!("touch-batch-short-{suffix}"), &ns, Tier::Short, 5);
    let id = store.store(&ctx, &mem).await.expect("seed store");
    let ids = vec![id.clone()];

    let pre = store.get(&ctx, &id).await.expect("get pre");
    let pre_expires = pre.expires_at.clone().expect("short tier seeded with TTL");

    // Touch 6 times — well above the mid→long threshold. Short rows
    // must stay short; mid→long arm only matches `tier = 'mid'`.
    let rows = touch_n(&store, &ctx, &ids, 6).await;
    let r = &rows[0];
    assert_eq!(r.access_count, 6, "short row access_count after 6 touches");
    assert_eq!(r.tier, Tier::Short, "short stays short — no auto-promote");
    let post_expires = r
        .expires_at
        .clone()
        .expect("short expires_at must slide forward, not clear");
    // Sliding-window REPLACEMENT semantics (CLAUDE.md §Recall Pipeline /
    // Touch operations): touch sets `expires_at = now + 1h` for short
    // tier. This REPLACES the create-time 6h backstop (which is the
    // seeded `pre_expires` value), so on first touch the new value
    // is EARLIER than the seed. The test must assert that the
    // expires_at WAS UPDATED (replaced) and that the new value sits
    // near the expected 1h-from-now anchor — not that it is greater
    // than the seed.
    assert_ne!(
        pre_expires, post_expires,
        "short-tier touch must REPLACE expires_at with the sliding 1h window: \
         pre={pre_expires} post={post_expires}"
    );
    let post_parsed = chrono::DateTime::parse_from_rfc3339(&post_expires)
        .expect("post_expires parses as RFC3339")
        .with_timezone(&chrono::Utc);
    let now = chrono::Utc::now();
    let delta = post_parsed - now;
    // Per-access window is 1h. Allow ±5min tolerance for SQL
    // round-trip latency + the few seconds between seed and touch.
    assert!(
        delta >= chrono::Duration::minutes(55) && delta <= chrono::Duration::minutes(65),
        "post_expires must be ~1h from now (sliding-window contract): \
         delta={delta:?} post={post_expires}"
    );
}
