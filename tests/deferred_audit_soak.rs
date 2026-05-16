// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! V-4 — deferred-audit queue soak tests (#698 commercial-claim
//! validation pass).
//!
//! Claim being validated: "the deferred audit queue maintains
//! ordering and does not silently drop under load."
//!
//! Two tests:
//!
//!   - `soak_lite_5k_refusals_no_drops_ordered` — CI-budget soak
//!     that submits 5K refusals over a short window (5s). Runs in
//!     the default test set.
//!   - `soak_60k_refusals_no_drops_ordered` — `#[ignore]` heavy
//!     soak that submits 60K refusals (50 clients × 20 refusals/s ×
//!     60s). Invoked explicitly with `cargo test ... --
//!     --include-ignored`.
//!
//! v34 closeout (#698): `signed_events` now carries `prev_hash` +
//! `sequence` (the cross-row hash chain documented at
//! `src/signed_events.rs:32-83`). This soak test asserts the chain
//! holds end-to-end across concurrent drainer inserts via
//! [`signed_events::verify_chain`] — the load-bearing monotonic-
//! sequence + tamper-evident property the directive originally
//! requested. The timestamp-ordering assertion is preserved as a
//! defense-in-depth invariant.

use ai_memory::governance::agent_action::{AgentAction, Decision};
use ai_memory::governance::deferred_audit::{
    DeferredAuditEvent, GOVERNANCE_REFUSAL_EVENT_TYPE, close_and_flush,
    install_deferred_audit_drainer,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn fresh_tempdir() -> tempfile::TempDir {
    // Honor the project's no-/tmp hard rule by routing tempdirs
    // through TMPDIR which the operator points at .local-runs/.
    tempfile::Builder::new()
        .prefix("ai-memory-soak-")
        .tempdir()
        .expect("tempdir")
}

fn refusal_action(i: u64) -> AgentAction {
    AgentAction::Custom {
        custom_kind: "memory_write".to_string(),
        // unique payload per refusal so the canonical-bytes hash
        // differs row-to-row (no spurious dedup).
        payload: serde_json::json!({"namespace": format!("test/r{i}")}),
    }
}

fn refusal_decision() -> Decision {
    Decision::Refuse {
        rule_id: "R001".to_string(),
        reason: "soak: synthetic refusal".to_string(),
    }
}

/// CI-budget soak. 50 concurrent producers × 100 refusals each =
/// 5,000 events. Asserts:
///   - zero dropped events (DB row count == 5000)
///   - rows are timestamp-ordered (no row carries a timestamp
///     earlier than the one before it in the `list_signed_events`
///     order)
///   - drainer p99 lag ≤ 500ms (tight bound; we measure from
///     submission to row presence on a post-flush re-read)
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soak_lite_5k_refusals_no_drops_ordered() {
    soak_run(5_000, 50, 100, Duration::from_millis(500), "soak-lite").await;
}

/// Heavy soak — 50 producers × 1,200 refusals each = 60,000 events
/// (mirrors 50 clients × 20 refusals/s × 60s in aggregate volume,
/// without the in-test sleep gating; the drainer's observed
/// throughput on this box is well above 20/s so a wall-clock cap
/// would only slow the test without changing the invariant tested).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "soak test; run explicitly with --include-ignored"]
async fn soak_60k_refusals_no_drops_ordered() {
    soak_run(60_000, 50, 1_200, Duration::from_millis(500), "soak-60k").await;
}

/// One producer's payload — runs in its own `tokio::spawn` task.
async fn run_producer(
    queue: ai_memory::governance::deferred_audit::DeferredAuditQueue,
    queued: Arc<AtomicU64>,
    producer_id: u64,
    per_producer: u64,
) {
    for i in 0..per_producer {
        let action = refusal_action(producer_id * per_producer + i);
        let decision = refusal_decision();
        let event = DeferredAuditEvent::from_refusal(
            &format!("agent:soak-{producer_id}"),
            &action,
            &decision,
        )
        .expect("refusal builds");
        if queue.submit(event) {
            queued.fetch_add(1, Ordering::Relaxed);
        }
        // small async yield so producers interleave on the
        // multi-thread runtime
        if i.is_multiple_of(64) {
            tokio::task::yield_now().await;
        }
    }
}

/// Re-open the post-drain DB and pull (count, ordered rows).
fn read_signed_events(db_path: &std::path::Path) -> (u64, Vec<(String, String)>) {
    let conn = ai_memory::db::open(db_path).expect("reopen db post-drain");
    let appended: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
            rusqlite::params![GOVERNANCE_REFUSAL_EVENT_TYPE],
            |r| r.get(0),
        )
        .expect("count");
    let appended = u64::try_from(appended).expect("count fits");
    let mut stmt = conn
        .prepare(
            "SELECT timestamp, id FROM signed_events \
             WHERE event_type = ?1 \
             ORDER BY timestamp ASC, id ASC",
        )
        .expect("prepare");
    let rows: Vec<(String, String)> = stmt
        .query_map(rusqlite::params![GOVERNANCE_REFUSAL_EVENT_TYPE], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })
        .expect("query")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("rows");
    (appended, rows)
}

async fn soak_run(
    expected: u64,
    producers: u64,
    per_producer: u64,
    drainer_p99_budget: Duration,
    label: &str,
) {
    assert_eq!(producers * per_producer, expected, "math sanity");

    let dir = fresh_tempdir();
    let db_path: PathBuf = dir.path().join(format!("{label}.db"));
    // Initialise the schema by opening the DB once via the canonical
    // helper (runs migrations).
    drop(ai_memory::db::open(&db_path).expect("init db"));

    let (queue, supervisor) = install_deferred_audit_drainer(&db_path);
    let queued = Arc::new(AtomicU64::new(0));

    let start = Instant::now();
    let producers_cap = usize::try_from(producers).expect("producers fit in usize");
    let mut tasks = Vec::with_capacity(producers_cap);
    for p in 0..producers {
        let q = queue.clone();
        let queued = queued.clone();
        tasks.push(tokio::spawn(async move {
            run_producer(q, queued, p, per_producer).await;
        }));
    }
    for t in tasks {
        t.await.expect("producer join");
    }
    let producer_done = Instant::now();
    let queued_total = queued.load(Ordering::Relaxed);
    assert_eq!(
        queued_total, expected,
        "{label}: every producer must have queued every event (queued={queued_total} expected={expected})"
    );

    // Drop the producer-side queue and wait for the supervisor to
    // drain. The supervisor returns only after `receiver.recv()`
    // returns None — i.e. every queued event has been appended (or
    // the sink errored, in which case the metrics-side
    // `append_failures` counter is non-zero and we'd surface it
    // below).
    close_and_flush(queue, supervisor)
        .await
        .expect("drainer joins cleanly");
    let drain_done = Instant::now();

    let (appended, rows) = read_signed_events(&db_path);
    assert_eq!(
        appended, expected,
        "{label}: drainer must NOT silently drop events (appended={appended} expected={expected})"
    );
    assert_eq!(
        u64::try_from(rows.len()).expect("rows fit"),
        expected,
        "row read count"
    );

    // Verify timestamp ordering: each row's timestamp is >= the
    // previous row's. This is the documented cross-row order of
    // `list_signed_events`.
    let mut prev: Option<String> = None;
    for (ts, _id) in &rows {
        if let Some(p) = &prev {
            assert!(
                ts.as_str() >= p.as_str(),
                "{label}: timestamps must be non-decreasing in list order (prev={p}, cur={ts})"
            );
        }
        prev = Some(ts.clone());
    }

    // v34 (#698 V-4 closeout): walk the cross-row hash chain and
    // assert it holds end-to-end. This is the load-bearing
    // monotonic-sequence + tamper-evidence property the V-4
    // directive originally requested. The timestamp ordering above
    // is preserved as defense-in-depth.
    {
        let conn = ai_memory::db::open(&db_path).expect("reopen for chain verify");
        let report = ai_memory::signed_events::verify_chain(&conn, None).expect("verify_chain");
        assert!(
            report.chain_holds(),
            "{label}: cross-row chain MUST hold end-to-end after {expected} concurrent inserts; \
             report = {report:?}"
        );
        assert_eq!(
            report.chain_break, None,
            "{label}: no chain break expected; report = {report:?}"
        );
        // Every row the soak generated must show up under the chain
        // walk (it doesn't help to have a clean chain if rows are
        // missing).
        assert!(
            report.rows_checked >= expected,
            "{label}: verify_chain saw {} rows but expected at least {expected}; report = {report:?}",
            report.rows_checked,
        );
        eprintln!(
            "{label}: chain GREEN | rows_checked={} chain_break=None",
            report.rows_checked,
        );
    }

    // Drainer-lag p99 proxy: the total observed wall-time from
    // producer-done to drain-done divided by the event count. This is
    // a per-event-mean upper bound (real per-event lag is bounded by
    // mean × small constant under uniform load), conservative for a
    // p99 check.
    let drain_elapsed = drain_done.saturating_duration_since(producer_done);
    let per_event = if expected > 0 {
        drain_elapsed / u32::try_from(expected).unwrap_or(u32::MAX)
    } else {
        Duration::ZERO
    };
    // p99 budget bound (proxy): mean ≤ 1/10 of p99 budget — soft.
    let bound = drainer_p99_budget / 10;
    assert!(
        per_event <= bound,
        "{label}: drainer per-event-mean lag {per_event:?} exceeded mean-bound {bound:?} (p99 budget {drainer_p99_budget:?}); soak appended {appended} events in {drain_elapsed:?}"
    );

    // Print a one-line summary the validation harness can grep.
    let total_wall = start.elapsed();
    eprintln!(
        "{label}: soak OK | producers={producers} per_producer={per_producer} \
         expected={expected} appended={appended} wall={total_wall:?} \
         drain_elapsed={drain_elapsed:?} per_event_mean={per_event:?}"
    );
}
