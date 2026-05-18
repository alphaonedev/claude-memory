// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Issue #830 — regression test pinning the sliding-window-REPLACE
//! semantics of the short-tier TTL touch operation.
//!
//! The short-tier TTL touch is a sliding-window **REPLACEMENT** —
//! `expires_at = now + SHORT_TTL_EXTEND_SECS` on every access, NOT
//! `max(old_expires_at, now + extend)`. A short-tier memory created
//! with `expires_at = create + 6h` will have `expires_at = access + 1h`
//! after one touch. The pre-fix `CLAUDE.md` text ("extend TTL") implied
//! max-of-old-and-new, which was wrong: a single recall right after
//! creation actually SHORTENS the TTL from 6h to 1h.

use ai_memory::db;
use ai_memory::models::{
    ConfidenceSource, MID_TTL_EXTEND_SECS, Memory, MemoryKind, SHORT_TTL_EXTEND_SECS, Tier,
    default_metadata,
};
use chrono::{Duration, Utc};

fn make_short_memory(expires_at: Option<String>) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Short,
        namespace: "lifecycle-test".to_string(),
        title: format!("lifecycle-row-{}", uuid::Uuid::new_v4()),
        content: "lifecycle test content".to_string(),
        tags: vec![],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at,
        metadata: default_metadata(),
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

/// **Issue #830** — pin the sliding-window-REPLACE semantics of the
/// short-tier TTL touch operation.
///
/// Scenario: create a short-tier memory with `expires_at = create + 6h`
/// (the documented backstop), then perform one access (the touch path),
/// and assert the new `expires_at` is ~now + 1h — NOT the original
/// 6h-from-create value, and NOT a max-of-old-and-new value.
///
/// The pre-fix CLAUDE.md wording said "extend TTL (1h short / 1d mid)"
/// which implied additive or max semantics. The actual code does:
///
/// ```ignore
/// expires_at = now + SHORT_TTL_EXTEND_SECS  // unconditional REPLACE
/// ```
///
/// so a memory just created with a 6h backstop has its expiry pulled
/// IN to 1h on the very first recall. That is the contract, and that is
/// what this test pins.
#[test]
fn lifecycle_short_tier_ttl_is_sliding_window_replace() {
    let conn = db::open(std::path::Path::new(":memory:")).expect("open in-memory db");

    // Create-time expiry = create + 6h (the documented backstop for
    // short-tier memories).
    let create_time = Utc::now();
    let backstop_expires_at = (create_time + Duration::hours(6)).to_rfc3339();
    let mem = make_short_memory(Some(backstop_expires_at.clone()));
    let id = db::insert(&conn, &mem).expect("insert short-tier memory");

    // Sanity: the row went in with the 6h backstop.
    let pre_touch = db::get(&conn, &id).unwrap().unwrap();
    assert_eq!(
        pre_touch.expires_at.as_deref(),
        Some(backstop_expires_at.as_str()),
        "pre-touch expires_at should equal the 6h backstop"
    );

    // Perform one touch (the same path that memory_recall + memory_get
    // hit when they mutate access_count).
    let touch_at = Utc::now();
    db::touch(&conn, &id, SHORT_TTL_EXTEND_SECS, MID_TTL_EXTEND_SECS).expect("touch");

    let post_touch = db::get(&conn, &id).unwrap().unwrap();
    let new_expires_str = post_touch
        .expires_at
        .as_deref()
        .expect("expires_at must still be set on short-tier post-touch");
    let new_expires = chrono::DateTime::parse_from_rfc3339(new_expires_str)
        .expect("expires_at must parse as RFC3339");

    // Expected: new_expires ≈ touch_at + 1h. The actual implementation
    // computes `now + extend` *inside* `touch()`, so we allow a generous
    // ±5s window to absorb scheduling jitter between our `touch_at`
    // sample and the `Utc::now()` call inside the function.
    let expected = touch_at + Duration::seconds(SHORT_TTL_EXTEND_SECS);
    let drift = (new_expires.with_timezone(&Utc) - expected)
        .num_milliseconds()
        .abs();
    assert!(
        drift < 5_000,
        "post-touch expires_at ({new_expires_str}) should be within 5s of \
         touch_at + 1h ({expected:?}); drift_ms={drift}"
    );

    // And critically: NOT max-of-old-and-new. The pre-touch backstop
    // was 6h-out from creation, so a max() semantics would have kept
    // it. We assert the post-touch value is STRICTLY LESS than the
    // pre-touch backstop, proving the implementation replaces (and in
    // fact shortens, when the access happens before the backstop
    // window has aged into the per-touch window).
    let backstop_parsed = chrono::DateTime::parse_from_rfc3339(&backstop_expires_at).unwrap();
    assert!(
        new_expires < backstop_parsed,
        "sliding-window REPLACE contract violated: post-touch expires_at \
         ({new_expires:?}) should be STRICTLY EARLIER than the 6h backstop \
         ({backstop_parsed:?}) — max-of-old-and-new is NOT the contract"
    );
}
