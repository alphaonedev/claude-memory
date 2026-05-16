// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0
//
// v0.7 Track G — Task G6: per-event-class hard timeouts.
//
// G2 (PR #563) shipped the 20-variant `HookEvent`. G3 (PR #567)
// shipped per-hook `timeout_ms` enforcement inside the executor. G5
// (PR #573) shipped `HookChain::fire` which iterates hooks in
// priority order. G6 stitches the bound on the *whole chain*: a
// hook chain firing on a hot event (recall, search, index) cannot
// collectively burn more wall-clock than the event class allows,
// even if individual `timeout_ms` knobs would otherwise sum past
// the budget.
//
// # Why a class deadline at all
//
// The v0.6.3 recall path holds a 50ms p95 budget. If three hooks
// each set `timeout_ms = 1_000` subscribe to `post_recall`, a
// single slow hook can blow the recall budget by 20×. Per-hook
// timeouts protect the *individual* hook from a runaway script;
// per-class timeouts protect the *operation* from the chain.
//
// # The four classes
//
// Per V0.7-EPIC §G6, every `HookEvent` lands in exactly one of:
//
//   * Write       — pre/post_store, pre/post_delete, pre/post_promote,
//                   pre/post_link, pre/post_consolidate,
//                   pre/post_governance_decision, pre_archive.
//                   5000ms class deadline. Writes are user-initiated
//                   and rarer than reads, so we tolerate a longer
//                   chain (PII redaction → policy gate → audit emit
//                   chains can legitimately exceed 1s).
//   * Read        — pre/post_recall, pre/post_search.
//                   2000ms class deadline. Reads are the hot path;
//                   the budget is generous enough for a real
//                   guardrail hook (token classifier, RBAC check) but
//                   below the 5s write ceiling.
//   * Index       — on_index_eviction.
//                   1000ms class deadline. Index events fire from a
//                   maintenance background loop; a slow chain there
//                   cascades into an HNSW build stall.
//   * Transcript  — pre/post_transcript_store.
//                   5000ms class deadline. Transcripts are user-
//                   initiated like writes, but can carry MB-scale
//                   payloads where compression / classification hooks
//                   plausibly take a second or more.
//
// # How the budget is plumbed into `HookChain::fire`
//
// `HookChain::fire` (in `chain.rs`) computes the class deadline at
// entry: `chain_deadline = Instant::now() + class_deadline_for(event)`.
// Before firing each hook it derives the per-hook budget as
// `min(chain_deadline - now, hook.timeout_ms)`. The executor
// already enforces `timeout_ms` via `tokio::time::timeout`; G6
// shrinks that knob on the fly when the chain itself is running out
// of room. If the chain budget is fully consumed before the next
// hook fires, the chain logs a warning, increments the
// `timeout_violations` counter, and treats the remaining hooks as
// fail-open `Allow` per G5's default `FailMode::Open` posture.
//
// # Doctor surface
//
// The chain accumulates a process-wide `timeout_violations` counter
// (one global atomic, since the chain is built per-event and torn
// down at end-of-fire — there's no per-chain home for state). The
// doctor's `--hooks` block reads it via [`timeout_violations_total`]
// and renders it alongside G3's existing `events_fired /
// events_dropped / mean_latency_us` row.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::events::HookEvent;

// ---------------------------------------------------------------------------
// EventClass — the four budget buckets
// ---------------------------------------------------------------------------

/// Coarse classification of a [`HookEvent`] for per-class deadline
/// enforcement.
///
/// `Copy + Hash` so it can be a `HashMap` key in downstream code
/// (today the deadline table is a `match`, not a map; the derive
/// cost is zero and keeps options open for the doctor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventClass {
    /// State-mutating events: store / delete / promote / link /
    /// consolidate / governance / archive.
    Write,
    /// Query events: recall / search. Hottest path; tightest
    /// non-index budget.
    Read,
    /// HNSW index lifecycle events. Background maintenance loop.
    Index,
    /// Transcript I-track events. Same 5s budget as writes; called
    /// out separately because the payload shape and call-site
    /// pressure profile differ.
    Transcript,
    /// G10: synchronous hot-path hooks that fire *inside* the recall
    /// p95 budget (50ms). Today's only inhabitant is
    /// [`HookEvent::PreRecallExpand`]; future synchronous hot-path
    /// hooks (e.g. a `pre_search_expand`) would join this class. The
    /// 50ms ceiling is below the v0.6.3 recall budget by design — a
    /// hook that can't return a decision in 50ms cannot be wired on
    /// the read path without blowing SLO.
    HotPath,
}

// ---------------------------------------------------------------------------
// Class deadlines — hardcoded per V0.7-EPIC §G6
// ---------------------------------------------------------------------------

/// Class deadline for [`EventClass::Write`].
pub const WRITE_CLASS_DEADLINE_MS: u64 = 5_000;
/// Class deadline for [`EventClass::Read`].
pub const READ_CLASS_DEADLINE_MS: u64 = 2_000;
/// Class deadline for [`EventClass::Index`].
pub const INDEX_CLASS_DEADLINE_MS: u64 = 1_000;
/// Class deadline for [`EventClass::Transcript`].
pub const TRANSCRIPT_CLASS_DEADLINE_MS: u64 = 5_000;
/// G10 — class deadline for [`EventClass::HotPath`] (synchronous
/// recall-budget hooks). 50ms = the v0.6.3 recall p95 budget; a
/// hook that runs longer would blow the SLO. The class deadline is
/// the *whole-chain* ceiling — individual hook `timeout_ms` may be
/// configured smaller.
pub const HOT_PATH_CLASS_DEADLINE_MS: u64 = 50;

// ---------------------------------------------------------------------------
// event_class — the canonical mapping
// ---------------------------------------------------------------------------

/// Map a [`HookEvent`] to its [`EventClass`]. Total over the 25
/// variants — the compiler's exhaustiveness check enforces the table
/// stays in sync if a 26th event ever lands.
#[must_use]
pub fn event_class(event: HookEvent) -> EventClass {
    match event {
        // Writes: state-mutating memory operations.
        HookEvent::PreStore
        | HookEvent::PostStore
        | HookEvent::PreDelete
        | HookEvent::PostDelete
        | HookEvent::PrePromote
        | HookEvent::PostPromote
        | HookEvent::PreLink
        | HookEvent::PostLink
        | HookEvent::PreConsolidate
        | HookEvent::PostConsolidate
        | HookEvent::PreGovernanceDecision
        | HookEvent::PostGovernanceDecision
        | HookEvent::PreArchive
        // v0.7.0 Task 6/8: reflect lifecycle fires on the write
        // path (the substrate inserts the new reflection memory +
        // N reflects_on links inside a single transaction).
        | HookEvent::PreReflect
        | HookEvent::PostReflect
        // v0.7.0 L1-7: compaction pipeline events are write-class
        // (the pass may delete source rows and insert a summary).
        | HookEvent::PreCompaction
        | HookEvent::OnCompactionRollback => EventClass::Write,
        // Reads: query path. Hot.
        HookEvent::PreRecall
        | HookEvent::PostRecall
        | HookEvent::PreSearch
        | HookEvent::PostSearch => EventClass::Read,
        // Index: HNSW lifecycle.
        HookEvent::OnIndexEviction => EventClass::Index,
        // Transcripts: I-track interop.
        HookEvent::PreTranscriptStore | HookEvent::PostTranscriptStore => EventClass::Transcript,
        // G10: synchronous hot-path query expansion (50ms budget).
        HookEvent::PreRecallExpand => EventClass::HotPath,
    }
}

/// The hardcoded class deadline (as a [`Duration`]) for `class`.
/// The `match` mirrors [`event_class`] inverse-style; a single
/// branch means the compiler inlines this to a constant load at
/// every call site.
#[must_use]
pub fn class_deadline(class: EventClass) -> Duration {
    Duration::from_millis(match class {
        EventClass::Write => WRITE_CLASS_DEADLINE_MS,
        EventClass::Read => READ_CLASS_DEADLINE_MS,
        EventClass::Index => INDEX_CLASS_DEADLINE_MS,
        EventClass::Transcript => TRANSCRIPT_CLASS_DEADLINE_MS,
        EventClass::HotPath => HOT_PATH_CLASS_DEADLINE_MS,
    })
}

/// Convenience wrapper: `class_deadline(event_class(event))`. Used
/// at `HookChain::fire` entry to compute the wall-clock ceiling on
/// the entire chain.
#[must_use]
pub fn class_deadline_for_event(event: HookEvent) -> Duration {
    class_deadline(event_class(event))
}

// ---------------------------------------------------------------------------
// Per-hook budget derivation
// ---------------------------------------------------------------------------

/// Compute the per-hook timeout budget (in milliseconds) given:
///
///   * `chain_deadline` — the absolute `Instant` at which the chain
///     itself runs out of room (set at `HookChain::fire` entry).
///   * `now`            — the `Instant` *just before* this hook fires;
///     the chain calls this between hooks so the per-hook budget
///     shrinks monotonically as earlier hooks consume time.
///   * `hook_timeout_ms` — the hook's own configured `timeout_ms`.
///
/// Returns `Some(budget_ms)` if the chain still has any time left,
/// `None` if the deadline has already passed (caller treats that as
/// a class-deadline trip — log warning, increment violation counter,
/// fail-open `Allow`).
///
/// The result is the smaller of the two budgets — the chain
/// deadline floor and the hook's own ceiling. `u32`-sized to match
/// `HookConfig.timeout_ms`; durations beyond `u32::MAX ms` (~49d)
/// would saturate, which is fine because the class deadlines are
/// in-the-low-seconds.
#[must_use]
pub fn per_hook_budget_ms(
    chain_deadline: Instant,
    now: Instant,
    hook_timeout_ms: u32,
) -> Option<u32> {
    if now >= chain_deadline {
        return None;
    }
    let remaining = chain_deadline.saturating_duration_since(now);
    let remaining_ms = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
    Some(remaining_ms.min(hook_timeout_ms))
}

// ---------------------------------------------------------------------------
// timeout_violations_total — process-wide counter
// ---------------------------------------------------------------------------

/// Process-wide count of class-deadline trips. Bumped by the chain
/// runner every time a hook's per-hook budget came back as `None`
/// (i.e. the class deadline expired before the hook even got to
/// fire) AND every time a hook returned an `ExecutorError::Timeout`
/// because the *shrunk* budget tripped inside the executor.
///
/// A global atomic (rather than a per-chain field) because:
///
///   * `HookChain` is built per-event and discarded at end-of-fire
///     — there's no long-lived home for the counter on the chain
///     itself.
///   * The `ExecutorRegistry` does have a long-lived per-hook
///     metrics struct, but timeout *violations* are a chain-level
///     concept (the executor only knows it tripped its own
///     `timeout_ms`; it doesn't know whether that was the
///     operator-configured ceiling or the chain-derived floor).
///   * `AtomicU64` is lock-free and the bump path is on the failure
///     branch only, so there's no measurable contention.
///
/// The doctor reads this via [`timeout_violations_total`] and
/// renders it next to G3's `events_fired / events_dropped` row.
static TIMEOUT_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// Increment the process-wide violation counter. Called by the
/// chain runner.
pub fn record_timeout_violation() {
    TIMEOUT_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
}

/// Snapshot of the process-wide violation counter. Read by the
/// doctor surface.
#[must_use]
pub fn timeout_violations_total() -> u64 {
    TIMEOUT_VIOLATIONS.load(Ordering::Relaxed)
}

/// Reset the violation counter. Test-only — production never
/// resets, since the doctor relies on a monotonic count to detect
/// "did we trip a budget since boot?".
#[cfg(test)]
pub fn reset_timeout_violations_for_test() {
    TIMEOUT_VIOLATIONS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `HookEvent` variant must classify into exactly one
    /// `EventClass`. Table-driven so adding a 26th variant without
    /// updating the mapping fails this test (the compiler also
    /// flags the missing arm in `event_class`, but the assertion
    /// surface here is what an operator reading the test reads).
    #[test]
    fn event_class_table_covers_all_25_variants() {
        let table = [
            // Write — 17 variants (Task 6/8 added pre_reflect + post_reflect;
            // L1-7 added pre_compaction + on_compaction_rollback).
            (HookEvent::PreStore, EventClass::Write),
            (HookEvent::PostStore, EventClass::Write),
            (HookEvent::PreDelete, EventClass::Write),
            (HookEvent::PostDelete, EventClass::Write),
            (HookEvent::PrePromote, EventClass::Write),
            (HookEvent::PostPromote, EventClass::Write),
            (HookEvent::PreLink, EventClass::Write),
            (HookEvent::PostLink, EventClass::Write),
            (HookEvent::PreConsolidate, EventClass::Write),
            (HookEvent::PostConsolidate, EventClass::Write),
            (HookEvent::PreGovernanceDecision, EventClass::Write),
            (HookEvent::PostGovernanceDecision, EventClass::Write),
            (HookEvent::PreArchive, EventClass::Write),
            (HookEvent::PreReflect, EventClass::Write),
            (HookEvent::PostReflect, EventClass::Write),
            (HookEvent::PreCompaction, EventClass::Write),
            (HookEvent::OnCompactionRollback, EventClass::Write),
            // Read — 4 variants.
            (HookEvent::PreRecall, EventClass::Read),
            (HookEvent::PostRecall, EventClass::Read),
            (HookEvent::PreSearch, EventClass::Read),
            (HookEvent::PostSearch, EventClass::Read),
            // Index — 1 variant.
            (HookEvent::OnIndexEviction, EventClass::Index),
            // Transcript — 2 variants.
            (HookEvent::PreTranscriptStore, EventClass::Transcript),
            (HookEvent::PostTranscriptStore, EventClass::Transcript),
            // HotPath — 1 variant (G10).
            (HookEvent::PreRecallExpand, EventClass::HotPath),
        ];

        assert_eq!(
            table.len(),
            25,
            "v0.7.0 L1-7 mapping must cover exactly the 25 HookEvent variants"
        );
        for (event, expected) in table {
            assert_eq!(
                event_class(event),
                expected,
                "event {event:?} mis-classified"
            );
        }
    }

    #[test]
    fn class_deadlines_match_epic_table() {
        assert_eq!(
            class_deadline(EventClass::Write),
            Duration::from_millis(5_000)
        );
        assert_eq!(
            class_deadline(EventClass::Read),
            Duration::from_millis(2_000)
        );
        assert_eq!(
            class_deadline(EventClass::Index),
            Duration::from_millis(1_000)
        );
        assert_eq!(
            class_deadline(EventClass::Transcript),
            Duration::from_millis(5_000)
        );
        // G10: hot-path budget is the v0.6.3 recall p95 (50ms).
        assert_eq!(
            class_deadline(EventClass::HotPath),
            Duration::from_millis(50)
        );
    }

    #[test]
    fn class_deadline_for_event_round_trips_through_class() {
        // Spot-check one variant per class.
        assert_eq!(
            class_deadline_for_event(HookEvent::PreStore),
            Duration::from_millis(WRITE_CLASS_DEADLINE_MS)
        );
        assert_eq!(
            class_deadline_for_event(HookEvent::PostRecall),
            Duration::from_millis(READ_CLASS_DEADLINE_MS)
        );
        assert_eq!(
            class_deadline_for_event(HookEvent::OnIndexEviction),
            Duration::from_millis(INDEX_CLASS_DEADLINE_MS)
        );
        assert_eq!(
            class_deadline_for_event(HookEvent::PostTranscriptStore),
            Duration::from_millis(TRANSCRIPT_CLASS_DEADLINE_MS)
        );
        // G10: PreRecallExpand is the inhabitant of HotPath.
        assert_eq!(
            class_deadline_for_event(HookEvent::PreRecallExpand),
            Duration::from_millis(HOT_PATH_CLASS_DEADLINE_MS)
        );
    }

    #[test]
    fn per_hook_budget_takes_minimum_of_chain_and_hook() {
        let now = Instant::now();
        let chain_deadline = now + Duration::from_millis(500);

        // Hook timeout is 200ms — chain has 500ms left, hook ceiling
        // wins → 200.
        let budget = per_hook_budget_ms(chain_deadline, now, 200).expect("not yet expired");
        assert_eq!(budget, 200);

        // Hook timeout is 5000ms — chain ceiling wins → ~500 (allow
        // 1ms slop because Instant::now() inside the function call
        // is a touch later than the test's `now`).
        let budget = per_hook_budget_ms(chain_deadline, now, 5_000).expect("not yet expired");
        assert!(
            (498..=500).contains(&budget),
            "expected ~500ms chain budget, got {budget}"
        );
    }

    #[test]
    fn per_hook_budget_returns_none_when_chain_deadline_passed() {
        let now = Instant::now();
        let chain_deadline = now - Duration::from_millis(1);
        assert!(per_hook_budget_ms(chain_deadline, now, 1_000).is_none());
    }

    #[test]
    fn per_hook_budget_at_exact_deadline_is_none() {
        let now = Instant::now();
        // `now >= chain_deadline` is the trip condition.
        assert!(per_hook_budget_ms(now, now, 1_000).is_none());
    }

    #[test]
    fn timeout_violations_counter_is_monotonic_and_resettable() {
        reset_timeout_violations_for_test();
        assert_eq!(timeout_violations_total(), 0);
        record_timeout_violation();
        record_timeout_violation();
        record_timeout_violation();
        assert_eq!(timeout_violations_total(), 3);
        reset_timeout_violations_for_test();
        assert_eq!(timeout_violations_total(), 0);
    }
}
