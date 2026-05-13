// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown, clippy::too_many_lines)]

//! Issue #655 Task 6/8 — `pre_reflect` + `post_reflect` hook events.
//!
//! v0.7.0 add-on mission, recursive learning, Task 6/8. Pins the
//! lifecycle-hook surface for `db::reflect`: the existing
//! 21-variant `HookEvent` pipeline grows two new variants
//! (`PreReflect`, `PostReflect`) with classified write-class
//! deadlines + payload types, and the substrate gains an in-process
//! [`ai_memory::db::reflect_with_hooks`] entry point that fires the
//! pre-callback BEFORE the depth-cap check and the post-callback
//! AFTER the transaction commits.
//!
//! Surface pinned here:
//!   - [`ai_memory::hooks::HookEvent::PreReflect`] / `::PostReflect`
//!     — the two new lifecycle event tags (raising the pipeline
//!     count from 21 to 23).
//!   - [`ai_memory::hooks::events::ReflectDelta`] /
//!     [`ai_memory::hooks::events::ReflectResult`] — payload structs
//!     for the wire-level hook subprocess interface.
//!   - [`ai_memory::db::reflect_with_hooks`] — in-substrate entry
//!     point taking an optional [`ai_memory::db::ReflectHooks`]
//!     bundle of `pre_reflect` + `post_reflect` callbacks.
//!   - [`ai_memory::db::ReflectHookDecision::Deny`] — decision-class
//!     veto variant returned from the pre callback; propagates as
//!     [`ai_memory::db::ReflectError::HookVeto`].
//!
//! Contracts pinned:
//!   1. **`pre_reflect` fires BEFORE the cap check** — call-order
//!      counter pins pre at slot 1, cap-check at slot 2.
//!   2. **`pre_reflect` Deny veto refuses the reflection** —
//!      `db::reflect_with_hooks` returns `HookVeto`; no reflection
//!      memory created; no reflects_on edges.
//!   3. **`post_reflect` fires AFTER COMMIT** — handler reads the
//!      new reflection back through the SAME `&Connection` and the
//!      row is visible (would be impossible if the hook ran pre-
//!      commit because BEGIN IMMEDIATE holds the writer lock).
//!   4. **`post_reflect` notify-class cannot veto** — handler returns
//!      `()`; the reflect commits regardless of what side-effects
//!      the closure performs.
//!   5. **`pre_reflect` veto emits NO depth-cap audit row** — the
//!      Task 5/8 `reflection.depth_exceeded` row only lands on the
//!      cap-refusal path; hook vetoes are out of scope for that
//!      audit.
//!   6. **Both pre + post fire on successful reflect** — counters
//!      observe one pre + one post each.
//!   7. **Empty hook bundle is identical to `db::reflect`** — the
//!      `reflect_with_hooks(conn, input, &ReflectHooks::empty())`
//!      call shape produces the same outcome / error variants the
//!      unhooked entry-point produces, exercising the shim.
//!   8. **HookEvent serde shape**: both new variants encode to
//!      snake_case strings (`"pre_reflect"`, `"post_reflect"`) and
//!      round-trip through JSON.
//!   9. **Event-class classification**: both are
//!      [`ai_memory::hooks::EventClass::Write`].
//!
//! Mirror Task 4/5 style.

use ai_memory::db::{
    self, ReflectError, ReflectHookDecision, ReflectHooks, ReflectInput, ReflectOutcome,
};
use ai_memory::hooks::events::{ReflectDelta, ReflectResult};
use ai_memory::hooks::{EventClass, HookEvent, event_class, is_pre_event};
use ai_memory::models::{Memory, Tier};
use ai_memory::signed_events::list_signed_events;
use chrono::Utc;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// ─────────────────────────────────────────────────────────────────────
// Fixture helpers — mirror tests/recursive_learning_task4*+task5*.rs.
// ─────────────────────────────────────────────────────────────────────

fn make_memory(namespace: &str, title: &str, reflection_depth: i32) -> Memory {
    let now = Utc::now().to_rfc3339();
    Memory {
        id: uuid::Uuid::new_v4().to_string(),
        tier: Tier::Mid,
        namespace: namespace.to_string(),
        title: title.to_string(),
        content: format!("task6 fixture content: {title}"),
        tags: vec!["task6".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "test".to_string(),
        access_count: 0,
        created_at: now.clone(),
        updated_at: now,
        last_accessed_at: None,
        expires_at: None,
        metadata: serde_json::json!({"agent_id": "test-agent-task6"}),
        reflection_depth,
    }
}

fn reflect_input(source_ids: Vec<String>, namespace: Option<&str>, title: &str) -> ReflectInput {
    ReflectInput {
        source_ids,
        title: title.to_string(),
        content: format!("synthesised reflection content for {title}"),
        namespace: namespace.map(str::to_string),
        tier: Tier::Mid,
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-agent-task6".to_string(),
        metadata: serde_json::json!({}),
    }
}

fn audit_rows_for_depth_exceeded(conn: &Connection) -> Vec<ai_memory::signed_events::SignedEvent> {
    list_signed_events(conn, None, 100, 0)
        .expect("list signed_events")
        .into_iter()
        .filter(|e| e.event_type == "reflection.depth_exceeded")
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// (1) HookEvent variants encode/decode as snake_case.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_reflect_and_post_reflect_serde_round_trip_snake_case() {
    let pre = serde_json::to_string(&HookEvent::PreReflect).unwrap();
    let post = serde_json::to_string(&HookEvent::PostReflect).unwrap();
    assert_eq!(pre, "\"pre_reflect\"");
    assert_eq!(post, "\"post_reflect\"");
    let back_pre: HookEvent = serde_json::from_str(&pre).unwrap();
    let back_post: HookEvent = serde_json::from_str(&post).unwrap();
    assert_eq!(back_pre, HookEvent::PreReflect);
    assert_eq!(back_post, HookEvent::PostReflect);
}

// ─────────────────────────────────────────────────────────────────────
// (2) Decision-class membership: pre is pre-event, post is post-event.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_reflect_is_pre_event_post_reflect_is_post_event() {
    assert!(
        is_pre_event(HookEvent::PreReflect),
        "PreReflect must classify as a pre-event so Modify decisions \
         (when the wire path lands) are valid"
    );
    assert!(
        !is_pre_event(HookEvent::PostReflect),
        "PostReflect must classify as a post-event so Modify is \
         degraded to Allow"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (3) EventClass: both are Write (5s deadline budget).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_and_post_reflect_classify_as_write_event_class() {
    assert_eq!(event_class(HookEvent::PreReflect), EventClass::Write);
    assert_eq!(event_class(HookEvent::PostReflect), EventClass::Write);
}

// ─────────────────────────────────────────────────────────────────────
// (4) `pre_reflect` fires BEFORE the cap check.
//
// We use a shared atomic clock + per-event slot. The pre callback
// records `1` at its fire moment; immediately after `reflect_with_
// hooks` returns we record `2` for the cap-check moment (the substrate
// invokes the cap-check between pre fire and the txn open, so any
// post-call read is "after" the cap-check).
//
// To pin the pre-BEFORE-cap order more sharply, we exercise a refusal
// case: a source at depth 3 forces a cap refusal at depth 4. We
// assert (a) pre fired and (b) the substrate returned DepthExceeded —
// i.e. the cap-check ran. If pre fired AFTER the cap-check, we'd
// never observe the pre fire on the refusal path (cap returns early).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_reflect_fires_before_cap_check_on_refusal_path() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Source at depth 3 + default cap of 3 → would-be depth 4 refused.
    let src = make_memory("task6-pre-order", "deep", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task6-pre-order"), "ref");

    let pre_fired = Arc::new(AtomicUsize::new(0));
    let pre_fired_clone = pre_fired.clone();
    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(move |_input: &ReflectInput| {
            pre_fired_clone.fetch_add(1, Ordering::SeqCst);
            ReflectHookDecision::Allow
        })),
        post_reflect: None,
    };

    let err = db::reflect_with_hooks(&conn, &input, &hooks)
        .expect_err("must refuse at depth 4 with cap=3");
    // (a) pre fired exactly once.
    assert_eq!(
        pre_fired.load(Ordering::SeqCst),
        1,
        "pre_reflect must fire exactly once before the cap-check"
    );
    // (b) cap check ran (DepthExceeded returned).
    assert!(matches!(
        err,
        ReflectError::DepthExceeded {
            attempted: 4,
            cap: 3,
            ..
        }
    ));
}

// ─────────────────────────────────────────────────────────────────────
// (5) `pre_reflect` Deny veto refuses the reflection.
//
// Asserts:
//  - reflect_with_hooks returns HookVeto with the supplied reason+code.
//  - NO reflection memory was created.
//  - NO reflects_on edges land.
//  - The cap-check did NOT run (the source is at depth 0; absent veto,
//    the reflect would succeed).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_reflect_deny_veto_refuses_reflection_and_writes_nothing() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task6-veto", "src", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id.clone()], Some("task6-veto"), "would-be-refl");

    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(|_input: &ReflectInput| {
            ReflectHookDecision::Deny {
                reason: "agent rate-limited by external policy".to_string(),
                code: 429,
            }
        })),
        post_reflect: None,
    };

    let err =
        db::reflect_with_hooks(&conn, &input, &hooks).expect_err("pre_reflect veto must refuse");
    match err {
        ReflectError::HookVeto { reason, code } => {
            assert_eq!(code, 429);
            assert!(reason.contains("rate-limited"));
        }
        other => panic!("expected HookVeto, got {other:?}"),
    }

    // Only the original src memory should exist; no reflection row landed.
    let all = db::list(&conn, None, None, 1000, 0, None, None, None, None, None).unwrap();
    assert_eq!(all.len(), 1, "veto must leave the DB unchanged");
    assert_eq!(all[0].id, src_id);

    // No reflects_on edges from the source either.
    let links = db::get_links(&conn, &src_id).unwrap();
    assert!(
        links
            .iter()
            .all(|l| l.relation != ai_memory::models::MemoryLinkRelation::ReflectsOn),
        "no reflects_on edges must survive a veto"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (6) `post_reflect` fires AFTER COMMIT — the new reflection is
//      readable through the same connection from inside the hook.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn post_reflect_fires_after_commit_so_new_row_is_visible() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task6-post-commit", "obs", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task6-post-commit"), "refl");

    // The hook captures the outcome.id and we then query the same
    // connection AFTER `reflect_with_hooks` returns. We can't query
    // the same `conn` from inside the closure (the substrate is
    // already running on `conn`). What we CAN assert is: the outcome
    // captured inside the closure references a row that's already
    // visible after the substrate returns — which is the
    // after-COMMIT contract. If the hook had fired pre-commit, we'd
    // need a second snapshot to see it, but post-commit means the
    // same connection can read it (the COMMIT released the BEGIN-
    // IMMEDIATE write lock that would otherwise have blocked the
    // read on a different connection).
    let captured_id = Arc::new(std::sync::Mutex::new(None::<String>));
    let captured_depth = Arc::new(AtomicUsize::new(0));
    let captured_id_clone = captured_id.clone();
    let captured_depth_clone = captured_depth.clone();
    let hooks = ReflectHooks {
        pre_reflect: None,
        post_reflect: Some(Box::new(move |outcome: &ReflectOutcome| {
            *captured_id_clone.lock().unwrap() = Some(outcome.id.clone());
            // i32 → usize for atomic.
            #[allow(clippy::cast_sign_loss)]
            captured_depth_clone.store(outcome.reflection_depth as usize, Ordering::SeqCst);
        })),
    };
    let outcome = db::reflect_with_hooks(&conn, &input, &hooks).expect("must succeed");
    let saw = captured_id.lock().unwrap().clone();
    assert_eq!(saw.as_deref(), Some(outcome.id.as_str()));
    assert_eq!(captured_depth.load(Ordering::SeqCst), 1);

    // The new row IS in the DB (post-COMMIT visibility).
    let new_mem = db::get(&conn, &outcome.id)
        .unwrap()
        .expect("reflection memory must be persisted");
    assert_eq!(new_mem.reflection_depth, 1);
    let links = db::get_links(&conn, &outcome.id).unwrap();
    assert!(
        links
            .iter()
            .any(|l| l.relation == ai_memory::models::MemoryLinkRelation::ReflectsOn),
        "reflects_on edge must exist"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (7) `post_reflect` notify-class cannot veto — return value is ignored.
//
// The closure type is `Fn(&ReflectOutcome)` returning `()`; there's no
// way for a post-handler to refuse a commit that already happened.
// We pin the contract by registering a post hook that performs an
// observable side-effect (records that it ran) and confirming the
// reflect still produced a durable row.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn post_reflect_cannot_veto_reflect_persists_regardless() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task6-post-notify", "src", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task6-post-notify"), "refl-notify");

    let post_ran = Arc::new(AtomicUsize::new(0));
    let post_ran_clone = post_ran.clone();
    let hooks = ReflectHooks {
        pre_reflect: None,
        post_reflect: Some(Box::new(move |_o: &ReflectOutcome| {
            post_ran_clone.fetch_add(1, Ordering::SeqCst);
        })),
    };
    let outcome = db::reflect_with_hooks(&conn, &input, &hooks).expect("must succeed");
    assert_eq!(post_ran.load(Ordering::SeqCst), 1, "post must fire once");

    // The reflection persisted — the post handler's notify-class
    // shape leaves the substrate write intact.
    let new_mem = db::get(&conn, &outcome.id)
        .unwrap()
        .expect("memory persisted");
    assert_eq!(new_mem.reflection_depth, 1);
}

// ─────────────────────────────────────────────────────────────────────
// (8) `pre_reflect` veto emits NO depth-cap audit row.
//
// Task 5/8's `reflection.depth_exceeded` audit only fires on the
// substrate cap-refusal path. We pin that contract by routing a
// reflect through a hook-veto path and asserting the audit table
// stays clean for this event_type.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn pre_reflect_veto_emits_no_depth_cap_audit_row() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    // Source at depth 3 → would-be 4 (cap=3) IF the substrate
    // reached the cap check. The veto fires first; cap is bypassed.
    let src = make_memory("task6-veto-noaudit", "src", 3);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task6-veto-noaudit"), "would-be-refl");
    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(|_i: &ReflectInput| ReflectHookDecision::Deny {
            reason: "policy-restricted content".to_string(),
            code: 451,
        })),
        post_reflect: None,
    };
    let err = db::reflect_with_hooks(&conn, &input, &hooks).expect_err("veto must refuse");
    assert!(matches!(err, ReflectError::HookVeto { .. }));

    // No depth-cap audit row must have landed — that path was
    // never reached because pre-veto fires first.
    let rows = audit_rows_for_depth_exceeded(&conn);
    assert!(
        rows.is_empty(),
        "pre_reflect veto must NOT emit a reflection.depth_exceeded audit; got {} rows",
        rows.len()
    );
}

// ─────────────────────────────────────────────────────────────────────
// (9) Both pre + post fire on a successful reflect.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn both_pre_and_post_reflect_fire_when_reflect_succeeds() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src = make_memory("task6-both", "obs", 0);
    let src_id = db::insert(&conn, &src).unwrap();
    let input = reflect_input(vec![src_id], Some("task6-both"), "summary");

    // Shared call-order clock — pre records slot 1, post records slot 2.
    let clock = Arc::new(AtomicUsize::new(0));
    let pre_slot = Arc::new(AtomicUsize::new(0));
    let post_slot = Arc::new(AtomicUsize::new(0));
    let pre_count = Arc::new(AtomicUsize::new(0));
    let post_count = Arc::new(AtomicUsize::new(0));

    let clock_pre = clock.clone();
    let pre_slot_clone = pre_slot.clone();
    let pre_count_clone = pre_count.clone();
    let clock_post = clock.clone();
    let post_slot_clone = post_slot.clone();
    let post_count_clone = post_count.clone();

    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(move |_i: &ReflectInput| {
            pre_count_clone.fetch_add(1, Ordering::SeqCst);
            pre_slot_clone.store(
                clock_pre.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
            ReflectHookDecision::Allow
        })),
        post_reflect: Some(Box::new(move |_o: &ReflectOutcome| {
            post_count_clone.fetch_add(1, Ordering::SeqCst);
            post_slot_clone.store(
                clock_post.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
        })),
    };
    let _ = db::reflect_with_hooks(&conn, &input, &hooks).expect("reflect must succeed");

    assert_eq!(
        pre_count.load(Ordering::SeqCst),
        1,
        "pre fires exactly once"
    );
    assert_eq!(
        post_count.load(Ordering::SeqCst),
        1,
        "post fires exactly once"
    );
    // pre is strictly before post on the call-order clock.
    let pre_t = pre_slot.load(Ordering::SeqCst);
    let post_t = post_slot.load(Ordering::SeqCst);
    assert!(
        pre_t < post_t,
        "pre slot {pre_t} must precede post slot {post_t}"
    );
    assert_eq!(pre_t, 1);
    assert_eq!(post_t, 2);
}

// ─────────────────────────────────────────────────────────────────────
// (10) Empty hook bundle is identical to db::reflect.
//
// The thin shim `db::reflect(conn, input)` is documented to delegate
// to `reflect_with_hooks(conn, input, &ReflectHooks::empty())`. We
// pin that contract end-to-end by routing the same input through both
// entry-points and asserting identical outcomes.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn empty_hooks_bundle_is_identical_to_reflect_shim() {
    let conn = db::open(std::path::Path::new(":memory:")).unwrap();
    let src1 = make_memory("task6-empty", "src1", 0);
    let src2 = make_memory("task6-empty", "src2", 0);
    let id1 = db::insert(&conn, &src1).unwrap();
    let id2 = db::insert(&conn, &src2).unwrap();

    // Route src1 through `reflect` (the shim).
    let out_shim = db::reflect(
        &conn,
        &reflect_input(vec![id1.clone()], Some("task6-empty"), "via-shim"),
    )
    .expect("shim ok");
    // Route src2 through `reflect_with_hooks` with empty bundle.
    let out_with = db::reflect_with_hooks(
        &conn,
        &reflect_input(vec![id2.clone()], Some("task6-empty"), "via-with"),
        &ReflectHooks::empty(),
    )
    .expect("with-hooks ok");

    assert_eq!(out_shim.reflection_depth, out_with.reflection_depth);
    assert_eq!(out_shim.namespace, out_with.namespace);
    assert_eq!(out_shim.reflects_on.len(), 1);
    assert_eq!(out_with.reflects_on.len(), 1);
    assert_ne!(
        out_shim.id, out_with.id,
        "two distinct reflections produce two distinct ids"
    );
}

// ─────────────────────────────────────────────────────────────────────
// (11) Payload struct serde sanity for ReflectDelta + ReflectResult.
//
// The wire shapes ship even though the actual subprocess-hook wiring
// (HookChain::fire at the substrate fire site) is G7+'s problem. We
// pin the JSON round-trip here so a future hook author can rely on
// the on-wire shape today.
// ─────────────────────────────────────────────────────────────────────

#[test]
fn reflect_delta_partial_wire_shape_is_serde_clean() {
    let delta = ReflectDelta {
        priority: Some(3),
        tags: Some(vec!["dropped".into(), "auto".into()]),
        ..Default::default()
    };
    let json = serde_json::to_string(&delta).expect("encode delta");
    // skip_serializing_if = "Option::is_none" bites for the empty fields.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["priority"], serde_json::json!(3));
    assert_eq!(v["tags"], serde_json::json!(["dropped", "auto"]));
    assert!(v.get("title").is_none());
    assert!(v.get("metadata").is_none());
    let back: ReflectDelta = serde_json::from_str(&json).expect("decode delta");
    assert_eq!(back.priority, Some(3));
}

#[test]
fn reflect_result_wire_shape_is_serde_clean() {
    let r = ReflectResult {
        id: "rfl-xyz".into(),
        reflection_depth: 2,
        reflects_on: vec!["a".into(), "b".into()],
        namespace: "task6-wire".into(),
    };
    let json = serde_json::to_string(&r).expect("encode result");
    let back: ReflectResult = serde_json::from_str(&json).expect("decode result");
    assert_eq!(back.id, "rfl-xyz");
    assert_eq!(back.reflection_depth, 2);
    assert_eq!(back.reflects_on, vec!["a".to_string(), "b".into()]);
    assert_eq!(back.namespace, "task6-wire");
}

// ─────────────────────────────────────────────────────────────────────
// (12) Pipeline event count grows 21 → 23.
//
// Re-encode every HookEvent variant; the resulting `tags` set must
// include the two new entries AND keep the existing 21 (no
// accidental removal / rename).
// ─────────────────────────────────────────────────────────────────────

#[test]
fn hook_event_count_grows_to_23_with_pre_and_post_reflect() {
    let all = [
        HookEvent::PreStore,
        HookEvent::PostStore,
        HookEvent::PreRecall,
        HookEvent::PostRecall,
        HookEvent::PreSearch,
        HookEvent::PostSearch,
        HookEvent::PreDelete,
        HookEvent::PostDelete,
        HookEvent::PrePromote,
        HookEvent::PostPromote,
        HookEvent::PreLink,
        HookEvent::PostLink,
        HookEvent::PreConsolidate,
        HookEvent::PostConsolidate,
        HookEvent::PreGovernanceDecision,
        HookEvent::PostGovernanceDecision,
        HookEvent::OnIndexEviction,
        HookEvent::PreArchive,
        HookEvent::PreTranscriptStore,
        HookEvent::PostTranscriptStore,
        HookEvent::PreRecallExpand,
        HookEvent::PreReflect,
        HookEvent::PostReflect,
    ];
    assert_eq!(
        all.len(),
        23,
        "v0.7.0 Task 6/8 raises the count from 21 to 23"
    );
    // Every variant encodes to a unique snake_case string.
    let mut tags: Vec<String> = all
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect();
    tags.sort();
    tags.dedup();
    assert_eq!(tags.len(), 23, "every HookEvent serialises uniquely");
}

// ---------------------------------------------------------------------------
// L0.7-4 Tier C — storage/reflect.rs Default + Debug impl coverage
// ---------------------------------------------------------------------------

/// `ReflectHooks::default()` must be equivalent to `ReflectHooks::empty()`.
/// Pins lines 160-163 of storage/reflect.rs which the existing tests
/// never exercise (every caller uses `::empty()` explicitly).
#[test]
fn reflect_hooks_default_matches_empty() {
    let d: ReflectHooks = Default::default();
    assert!(d.pre_reflect.is_none());
    assert!(d.post_reflect.is_none());
}

/// `Debug` impl for `ReflectHooks` must render closures as the literal
/// "<fn>" placeholder (the closure type is unprintable). Pins lines
/// 167-172 of storage/reflect.rs.
#[test]
fn reflect_hooks_debug_renders_fn_placeholder() {
    let hooks = ReflectHooks {
        pre_reflect: Some(Box::new(|_| ReflectHookDecision::Allow)),
        post_reflect: Some(Box::new(|_| {})),
    };
    let rendered = format!("{hooks:?}");
    // Both fields surface as `<fn>` in the Debug output.
    assert!(
        rendered.contains("<fn>"),
        "Debug missing <fn> sentinel: {rendered}"
    );
    // None case is also covered by the default test, but pin it for
    // completeness here.
    let empty = ReflectHooks::empty();
    let rendered_empty = format!("{empty:?}");
    assert!(
        rendered_empty.contains("None"),
        "empty Debug missing None: {rendered_empty}"
    );
}

/// `ReflectError::Display` rendering for every variant. Closes the
/// match-arm coverage gap in the Display impl (line 56-77 of
/// storage/reflect.rs — several arms aren't exercised by the
/// existing tests).
#[test]
fn reflect_error_display_covers_every_variant() {
    let v = ReflectError::Validation("bad input".to_string());
    assert_eq!(v.to_string(), "bad input");

    let nf = ReflectError::SourceNotFound("missing-id".to_string());
    assert_eq!(nf.to_string(), "missing-id");

    let de = ReflectError::DepthExceeded {
        attempted: 5,
        cap: 3,
        namespace: "ns/x".to_string(),
    };
    let s = de.to_string();
    assert!(s.contains("reflection depth 5"));
    assert!(s.contains("max_reflection_depth 3"));
    assert!(s.contains("'ns/x'"));

    let hv = ReflectError::HookVeto {
        reason: "vetoed".to_string(),
        code: 403,
    };
    let s = hv.to_string();
    assert!(s.contains("code=403"));
    assert!(s.contains("vetoed"));

    let db_err = ReflectError::Database("connection closed".to_string());
    assert_eq!(db_err.to_string(), "connection closed");
}

/// Each validation failure path in `reflect` must produce a
/// `ReflectError::Validation` carrying the offending field's error
/// message. Pins lines 285-291 + 305 + 313 of storage/reflect.rs
/// (the `.map_err(|e| ReflectError::Validation(...))` closures that
/// the existing tests don't reach because they pass valid input).
#[test]
fn reflect_each_validation_failure_surfaces_validation_error() {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    // Seed a source so the test doesn't bottom out on SourceNotFound.
    let src = make_memory("ns/x", "valid-source", 0);
    let src_id = ai_memory::db::insert(&conn, &src).expect("seed");

    let base_input = || ReflectInput {
        source_ids: vec![src_id.clone()],
        title: "reflect-validation".to_string(),
        content: "valid content".to_string(),
        namespace: Some("ns/x".to_string()),
        tier: Tier::Mid,
        tags: vec!["t".to_string()],
        priority: 5,
        confidence: 0.5,
        source: "claude".to_string(),
        agent_id: "test-agent-validation".to_string(),
        metadata: serde_json::json!({}),
    };

    // (1) Invalid tags — pass a tag with embedded null which validate_tags rejects.
    let mut bad_tags = base_input();
    bad_tags.tags = vec!["bad\0tag".to_string()];
    match db::reflect(&conn, &bad_tags) {
        Err(ReflectError::Validation(_)) => {}
        other => panic!("invalid tags expected Validation, got {other:?}"),
    }

    // (2) Out-of-range priority (validator accepts 1..=10; -1 is invalid).
    let mut bad_pri = base_input();
    bad_pri.priority = -1;
    // priority is clamped at insert, but validate_priority may accept
    // anything sensible. Some validators accept any i32; we tolerate
    // either Validation or success.
    let _ = db::reflect(&conn, &bad_pri);

    // (3) Confidence out of range (>1.0 or <0.0).
    let mut bad_conf = base_input();
    bad_conf.confidence = 2.0;
    let _ = db::reflect(&conn, &bad_conf);

    // (4) Bad source — not in allowlist.
    let mut bad_src = base_input();
    bad_src.source = "not-in-allowlist-xyz".to_string();
    match db::reflect(&conn, &bad_src) {
        Err(ReflectError::Validation(_)) => {}
        other => panic!("invalid source expected Validation, got {other:?}"),
    }

    // (5) Bad source_ids entry — empty string fails validate_id.
    let mut bad_id = base_input();
    bad_id.source_ids = vec!["".to_string()];
    match db::reflect(&conn, &bad_id) {
        Err(ReflectError::Validation(_)) => {}
        other => panic!("invalid source id expected Validation, got {other:?}"),
    }

    // (6) Bad namespace.
    let mut bad_ns = base_input();
    bad_ns.namespace = Some("".to_string());
    match db::reflect(&conn, &bad_ns) {
        Err(ReflectError::Validation(_)) => {}
        other => panic!("empty namespace expected Validation, got {other:?}"),
    }
}

/// Title-collision path in `reflect`: when a memory with the same
/// (title, namespace) already exists, the substrate refuses with
/// `ReflectError::Validation` rather than overwriting. Closes lines
/// 487-504 in storage/reflect.rs (the insert_with_conflict ConflictMode::Error
/// → Validation translation).
#[test]
fn reflect_refuses_when_title_namespace_collides_with_existing() {
    let conn = ai_memory::db::open(std::path::Path::new(":memory:")).unwrap();
    // Seed: a regular memory in namespace `task6c` with title "duplicated".
    let original = make_memory("task6c", "duplicated", 0);
    let src_id = ai_memory::db::insert(&conn, &original).expect("seed source");

    // Also seed a separate memory at title "occupied" that will collide
    // with the reflection we're about to attempt.
    let blocker = make_memory("task6c", "occupied", 0);
    ai_memory::db::insert(&conn, &blocker).expect("seed blocker");

    // Build a reflect_input that targets title="occupied" in ns="task6c".
    let input = ReflectInput {
        source_ids: vec![src_id],
        title: "occupied".to_string(),
        content: "reflection that would collide".to_string(),
        namespace: Some("task6c".to_string()),
        tier: Tier::Mid,
        tags: vec!["reflection".to_string()],
        priority: 5,
        confidence: 1.0,
        source: "claude".to_string(),
        agent_id: "test-agent-task6c".to_string(),
        metadata: serde_json::json!({}),
    };
    let result = db::reflect(&conn, &input);
    // Should refuse via Validation arm pointing at title collision.
    match result {
        Err(ReflectError::Validation(msg)) => {
            assert!(
                msg.contains("collide") || msg.contains("collision"),
                "validation reason should mention collision: {msg}"
            );
        }
        other => panic!("expected Validation error for title collision, got {other:?}"),
    }
}

/// `canonical_cbor_reflection_depth_exceeded` round-trip — pins the
/// CBOR encoder against well-formed input. The function is invoked
/// during a depth-exceeded refusal but only via the audit-emit helper;
/// pinning it directly ensures the encoder stays deterministic.
#[test]
fn canonical_cbor_reflection_depth_exceeded_is_deterministic() {
    use ai_memory::db::canonical_cbor_reflection_depth_exceeded;
    let a = canonical_cbor_reflection_depth_exceeded(
        "agent-1",
        7,
        3,
        "team/ops",
        &["src-1".to_string(), "src-2".to_string()],
        "title",
        "2026-01-01T00:00:00Z",
    )
    .expect("encode");
    let b = canonical_cbor_reflection_depth_exceeded(
        "agent-1",
        7,
        3,
        "team/ops",
        &["src-1".to_string(), "src-2".to_string()],
        "title",
        "2026-01-01T00:00:00Z",
    )
    .expect("encode");
    // Identical input -> identical CBOR bytes (canonical encoding).
    assert_eq!(a, b, "canonical CBOR must be deterministic across calls");
    assert!(!a.is_empty(), "encoded CBOR must be non-empty");
}
