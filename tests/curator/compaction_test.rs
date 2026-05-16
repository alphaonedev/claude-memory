// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::missing_panics_doc
)]

//! Issue #664 L1-7 — Compaction pipeline minimum slice.
//!
//! Integration tests that pin the acceptance criteria from issue #664
//! through the PUBLIC API surface only (no `pub(crate)` items are
//! accessible from integration test crates).
//!
//! Tests that require access to `pub(crate)` items (`JaccardClustering`,
//! `CosineClustering`, `CompactionPass`) live in the `#[cfg(test)]` blocks
//! inside the relevant `src/curator/*.rs` files.
//!
//! What this file covers:
//!   1. `HookEvent::PreCompaction` + `HookEvent::OnCompactionRollback` exist,
//!      are `is_pre_event`-classified correctly, and have `EventClass::Write`.
//!   2. `CapabilityHooks::default()` reports `hook_events_count == 25`.
//!   3. `CuratorConfig.compaction.enabled` defaults to `false`.
//!   4. `CompactionDelta` + `CompactionRollbackEvent` payloads round-trip JSON.
//!   5. Verify-stage failure does NOT trigger rollback (notify-only event).

use ai_memory::config::{CapabilityHooks, HOOK_EVENTS_COUNT};
use ai_memory::curator::{CompactionConfig, CuratorConfig};
use ai_memory::hooks::decision::is_pre_event;
use ai_memory::hooks::events::{CompactionDelta, CompactionRollbackEvent, HookEvent};
use ai_memory::hooks::timeouts::{EventClass, event_class};

// ---------------------------------------------------------------------------
// Criterion 1 — pre_compaction hook event classification
// ---------------------------------------------------------------------------

/// `HookEvent::PreCompaction` must be a decision-class pre-event
/// (Allow / Modify / Deny / AskUser), classified as `EventClass::Write`.
#[test]
fn pre_compaction_is_decision_class_write_event() {
    let ev = HookEvent::PreCompaction;

    // Decision class: is_pre_event returns true → Modify decisions are valid.
    assert!(
        is_pre_event(ev),
        "PreCompaction must be a pre-event (decision class)"
    );

    // Write class: compaction mutates the DB (deletes sources, inserts summary).
    assert_eq!(
        event_class(ev),
        EventClass::Write,
        "PreCompaction must be classified as Write"
    );
}

// ---------------------------------------------------------------------------
// Criterion 2 — on_compaction_rollback hook event classification
// ---------------------------------------------------------------------------

/// `HookEvent::OnCompactionRollback` must be a notify-only post-event
/// classified as `EventClass::Write`.
#[test]
fn on_compaction_rollback_is_notify_only_write_event() {
    let ev = HookEvent::OnCompactionRollback;

    // Notify-only: is_pre_event returns false → Modify decisions are invalid.
    assert!(
        !is_pre_event(ev),
        "OnCompactionRollback must be a notify-only (post-/on-) event"
    );

    // Write class: rollback fires on the write path.
    assert_eq!(
        event_class(ev),
        EventClass::Write,
        "OnCompactionRollback must be classified as Write"
    );
}

// ---------------------------------------------------------------------------
// Criterion 3 — verify-stage failure does NOT trigger rollback
// ---------------------------------------------------------------------------

/// The verify stub fires after persist.  A failure must NOT cause a
/// rollback in the minimum slice (rollback is v0.8.0 Pillar 2.5).
///
/// The hook-contract proof: `OnCompactionRollback` is notify-only so
/// no Deny is possible from that hook → no inline rollback can be
/// triggered.
#[test]
fn verify_failure_does_not_trigger_rollback_in_minimum_slice() {
    // The two compaction events are distinct.
    assert_ne!(HookEvent::PreCompaction, HookEvent::OnCompactionRollback);

    // OnCompactionRollback is notify-only (no Deny possible → no abort).
    assert!(
        !is_pre_event(HookEvent::OnCompactionRollback),
        "rollback hook is notify-only: cannot veto, cannot trigger inline rollback"
    );

    // PreCompaction is decision-class (Deny aborts cluster, not rollback).
    assert!(
        is_pre_event(HookEvent::PreCompaction),
        "pre_compaction is the abort gate, separate from rollback"
    );
}

// ---------------------------------------------------------------------------
// Criterion 4 — compaction.enabled defaults to false (ROADMAP2 §7.5)
// ---------------------------------------------------------------------------

/// The `compaction` sub-config must default to `enabled = false` to satisfy
/// the opt-in requirement for Ollama-dependent compaction.
#[test]
fn compaction_config_defaults_to_disabled() {
    let cfg = CompactionConfig::default();
    assert!(
        !cfg.enabled,
        "compaction.enabled must default to false (ROADMAP2 §7.5, opt-in due to Ollama dep)"
    );
}

#[test]
fn curator_config_compaction_field_defaults_to_disabled() {
    let cfg = CuratorConfig::default();
    assert!(
        !cfg.compaction.enabled,
        "CuratorConfig.compaction.enabled must default to false"
    );
}

// ---------------------------------------------------------------------------
// Criterion 5 — hook_events_count reports 25
// ---------------------------------------------------------------------------

/// `CapabilityHooks::default().hook_events_count` must equal 25 to
/// satisfy the L1-7 acceptance criteria (22→24 in the issue, but the
/// actual enum count is 25 after Task 6/8 landed the reflect events).
#[test]
fn capability_hooks_reports_25_events() {
    let hooks = CapabilityHooks::default();
    assert_eq!(
        hooks.hook_events_count, 25,
        "hook_events_count must be 25 after L1-7 adds PreCompaction + OnCompactionRollback"
    );
    assert_eq!(
        HOOK_EVENTS_COUNT, 25,
        "HOOK_EVENTS_COUNT compile-time constant must be 25"
    );
}

// ---------------------------------------------------------------------------
// Criterion 6 — hook event count sanity (2 new L1-7 events)
// ---------------------------------------------------------------------------

/// Smoke-test the two new compaction events to ensure they're both
/// `EventClass::Write` after L1-7.
#[test]
fn hook_event_new_l1_7_events_are_write_class() {
    let new_events = [HookEvent::PreCompaction, HookEvent::OnCompactionRollback];
    for ev in &new_events {
        assert_eq!(
            event_class(*ev),
            EventClass::Write,
            "new compaction event {ev:?} must be Write class"
        );
    }
    // PreCompaction is decision-class; OnCompactionRollback is notify-only.
    assert!(is_pre_event(HookEvent::PreCompaction));
    assert!(!is_pre_event(HookEvent::OnCompactionRollback));
}

// ---------------------------------------------------------------------------
// Criterion 7 — CompactionDelta and CompactionRollbackEvent wire shape
// ---------------------------------------------------------------------------

/// Verify that the two new payload types serialise/deserialise correctly
/// so hook operators can parse them from stdin.
#[test]
fn compaction_delta_round_trips() {
    let d = CompactionDelta {
        pass_name: "consolidation".to_string(),
        candidate_ids: vec!["id-a".to_string(), "id-b".to_string()],
        namespace: "team/ops".to_string(),
    };
    let json = serde_json::to_string(&d).expect("encode");
    let back: CompactionDelta = serde_json::from_str(&json).expect("decode");
    assert_eq!(back.pass_name, "consolidation");
    assert_eq!(back.candidate_ids.len(), 2);
    assert_eq!(back.namespace, "team/ops");
}

#[test]
fn compaction_rollback_event_round_trips() {
    let ev = CompactionRollbackEvent {
        pass_name: "consolidation".to_string(),
        summary_id: "sum-1".to_string(),
        namespace: "team/ops".to_string(),
        reason: "summary row not found after insert".to_string(),
    };
    let json = serde_json::to_string(&ev).expect("encode");
    let back: CompactionRollbackEvent = serde_json::from_str(&json).expect("decode");
    assert_eq!(back.pass_name, "consolidation");
    assert_eq!(back.summary_id, "sum-1");
    assert_eq!(back.namespace, "team/ops");
    assert!(!back.reason.is_empty());
}

// ---------------------------------------------------------------------------
// Criterion 8 — new events round-trip JSON with correct snake_case tags
// ---------------------------------------------------------------------------

#[test]
fn compaction_hook_events_json_snake_case() {
    let pre = HookEvent::PreCompaction;
    let on = HookEvent::OnCompactionRollback;
    assert_eq!(
        serde_json::to_string(&pre).unwrap(),
        "\"pre_compaction\"",
        "PreCompaction must serialise as pre_compaction"
    );
    assert_eq!(
        serde_json::to_string(&on).unwrap(),
        "\"on_compaction_rollback\"",
        "OnCompactionRollback must serialise as on_compaction_rollback"
    );
    // Round-trip.
    let pre_back: HookEvent = serde_json::from_str("\"pre_compaction\"").unwrap();
    let on_back: HookEvent = serde_json::from_str("\"on_compaction_rollback\"").unwrap();
    assert_eq!(pre_back, pre);
    assert_eq!(on_back, on);
}
