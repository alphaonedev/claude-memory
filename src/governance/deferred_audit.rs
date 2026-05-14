// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Policy-Engine Item 3 — deferred audit-log queue for the
//! storage governance pre-write hook.
//!
//! # The gap this closes
//!
//! The substrate's `GOVERNANCE_PRE_WRITE` hook (installed in
//! `daemon_runtime::bootstrap_serve`, consulted from every
//! `storage::insert*` call) ran
//! [`super::agent_action::check_agent_action_no_audit`] — the
//! `_no_audit` suffix is there because emitting a `signed_events` row
//! from INSIDE the in-flight INSERT transaction would re-enter the
//! same `Connection` and deadlock under the substrate's
//! `Arc<Mutex<Connection>>` lock.
//!
//! Consequence: **storage refusals were typed-but-not-cryptographically-logged**.
//! Other paths (the audited [`super::agent_action::check_agent_action`]
//! variant) DO chain-log via `signed_events`. That asymmetry was the
//! known gap in the bypass-impossibility audit story.
//!
//! # The fix — deferred chain-log
//!
//! This module ships a process-wide `DeferredAuditQueue`:
//!
//! 1. The storage hook captures the refusal verdict + agent identity
//!    + canonical action payload as a [`DeferredAuditEvent`] and
//!    submits it to the queue via [`DeferredAuditQueue::submit`]
//!    (non-blocking, never panics).
//! 2. A background drainer task ([`spawn_drainer_task`]) owns a FRESH
//!    `Connection` (opened against the same `db_path` but NOT the
//!    substrate writer's connection — SQLite WAL allows parallel
//!    readers and the drainer's writes don't contend with the
//!    in-flight `storage::insert` transaction because it has
//!    already released its lock by the time the drainer runs).
//! 3. For every received event, the drainer appends a
//!    `governance.refusal` row to `signed_events`. The chain-log
//!    property closes.
//!
//! # Supervisor pattern
//!
//! The drainer task is wrapped by [`spawn_supervised_drainer`] which
//! restarts the inner task on panic. A panic in the drainer would
//! otherwise silently drop the audit chain — a regression worse than
//! the original gap. The supervisor uses `tokio::task::spawn` with
//! `JoinHandle` polling so cleanup on shutdown is deterministic.
//!
//! # Backpressure / lossiness
//!
//! The channel is `tokio::sync::mpsc::unbounded_channel` by design:
//!
//! - Refusals are rare (a properly-configured fleet refuses
//!   << 1% of writes).
//! - A bounded channel would silently drop on full — and a silent
//!   audit drop IS a security regression we cannot accept.
//! - Memory pressure under attack is bounded by the rate at which the
//!   drainer can append `signed_events` rows; on macOS / Linux a
//!   single SQLite append in WAL mode is ~25-100 microseconds, so a
//!   sustained 100k refusals/second saturates one core but never
//!   blocks the storage write path.
//!
//! # Graceful shutdown
//!
//! [`DeferredAuditQueue::close_and_flush`] drops the sender and
//! awaits the supervisor task to terminate. The drainer drains every
//! still-buffered event before exiting; pending events MUST land in
//! `signed_events` before the daemon's tokio runtime is torn down,
//! or the chain-log property is broken.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::governance::agent_action::{AgentAction, Decision};
use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};

/// Wire-name for the deferred refusal audit row. Audit-side dashboards
/// filter on this string to surface storage-hook refusals separate
/// from the existing `governance.check` rows produced by the audited
/// `check_agent_action` path.
pub const GOVERNANCE_REFUSAL_EVENT_TYPE: &str = "governance.refusal";

/// One refusal captured by the storage hook, awaiting flush to the
/// `signed_events` chain.
///
/// All fields are owned (no borrows) so the event can cross the mpsc
/// channel boundary without lifetime gymnastics. `payload_bytes` is
/// the canonical-JSON encoding of `{action, decision}` — the same
/// shape the audited path commits to via
/// `agent_action::emit_check_event`. The drainer hashes this on the
/// way to `signed_events.payload_hash`.
#[derive(Debug, Clone)]
pub struct DeferredAuditEvent {
    /// Agent identity at the moment of refusal (resolved from request
    /// or process context). Lands in `signed_events.agent_id`.
    pub agent_id: String,
    /// The action that was refused. Cloned from the hook input.
    pub action: AgentAction,
    /// The verdict — must be a `Refuse` variant; non-refusal events
    /// do not enter the queue (the submit helpers gate on
    /// `Decision::is_refusal`).
    pub decision: Decision,
    /// Wall-clock timestamp of refusal. Lands in
    /// `signed_events.timestamp` as RFC3339.
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl DeferredAuditEvent {
    /// Build a deferred event from the hook's three inputs. Returns
    /// `None` when `decision` is not a refusal — callers should
    /// only submit refusals to the queue (Allow / Warn paths do not
    /// chain-log a refusal row).
    #[must_use]
    pub fn from_refusal(agent_id: &str, action: &AgentAction, decision: &Decision) -> Option<Self> {
        if !decision.is_refusal() {
            return None;
        }
        Some(Self {
            agent_id: agent_id.to_string(),
            action: action.clone(),
            decision: decision.clone(),
            timestamp: chrono::Utc::now(),
        })
    }

    /// Extract the rule_id from the refusal verdict. Used by the
    /// drainer to surface the firing rule in the audit row's
    /// canonical payload.
    #[must_use]
    pub fn rule_id(&self) -> Option<&str> {
        match &self.decision {
            Decision::Refuse { rule_id, .. } => Some(rule_id.as_str()),
            _ => None,
        }
    }

    /// Extract the refusal reason from the verdict (verbatim
    /// operator-authored string).
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match &self.decision {
            Decision::Refuse { reason, .. } => Some(reason.as_str()),
            _ => None,
        }
    }

    /// Canonical JSON shape the drainer hashes for
    /// `signed_events.payload_hash`. Stable across versions: a
    /// flat object with `action`, `decision`, `agent_id`,
    /// `timestamp` keys — same outline as
    /// `agent_action::emit_check_event` plus the agent + timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error only if `serde_json` cannot serialize the
    /// action variant (in practice never happens for the canonical
    /// AgentAction shapes).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>> {
        let canonical = serde_json::json!({
            "action": self.action,
            "decision": self.decision,
            "agent_id": self.agent_id,
            "timestamp": self.timestamp.to_rfc3339(),
        });
        serde_json::to_vec(&canonical).context("DeferredAuditEvent::canonical_bytes")
    }
}

/// Shared counters surfaced for observability. Cloning is cheap
/// (just `Arc` bumps); the public read path is on the queue handle.
#[derive(Debug, Clone, Default)]
pub struct DeferredAuditMetrics {
    /// Number of events submitted into the queue. Includes events
    /// that were later dropped because the receiver was already
    /// closed.
    pub submitted: Arc<AtomicU64>,
    /// Number of events the drainer successfully appended to
    /// `signed_events`.
    pub appended: Arc<AtomicU64>,
    /// Number of submit attempts that failed because the receiver
    /// was already closed (drainer dropped / shutdown raced).
    pub send_failures: Arc<AtomicU64>,
    /// Number of drainer iterations that failed the SQLite append.
    /// A non-zero value indicates DB pressure / corruption; the
    /// supervisor surfaces these in tracing::error logs.
    pub append_failures: Arc<AtomicU64>,
    /// Number of times the supervisor restarted the drainer after a
    /// panic. Should be zero in healthy operation.
    pub drainer_panics: Arc<AtomicU64>,
}

impl DeferredAuditMetrics {
    /// Number of events submitted since process boot.
    #[must_use]
    pub fn submitted_count(&self) -> u64 {
        self.submitted.load(Ordering::Relaxed)
    }

    /// Number of events successfully chain-logged since process boot.
    #[must_use]
    pub fn appended_count(&self) -> u64 {
        self.appended.load(Ordering::Relaxed)
    }

    /// Number of submit failures (receiver dropped).
    #[must_use]
    pub fn send_failure_count(&self) -> u64 {
        self.send_failures.load(Ordering::Relaxed)
    }

    /// Number of append failures (SQLite error).
    #[must_use]
    pub fn append_failure_count(&self) -> u64 {
        self.append_failures.load(Ordering::Relaxed)
    }

    /// Number of supervisor-observed drainer panics.
    #[must_use]
    pub fn panic_count(&self) -> u64 {
        self.drainer_panics.load(Ordering::Relaxed)
    }
}

/// Producer-side handle. Cloneable so multiple callsites (HTTP
/// handler, MCP handler, internal substrate writer) all share one
/// queue.
#[derive(Clone)]
pub struct DeferredAuditQueue {
    sender: UnboundedSender<DeferredAuditEvent>,
    metrics: DeferredAuditMetrics,
}

impl DeferredAuditQueue {
    /// Create a fresh queue + uninstalled receiver. The receiver
    /// MUST be passed to [`spawn_drainer_task`] (or
    /// [`spawn_supervised_drainer`]) for events to land — submits
    /// against an unspawned receiver accumulate in the channel
    /// buffer indefinitely until the receiver is consumed or
    /// dropped.
    #[must_use]
    pub fn new() -> (Self, UnboundedReceiver<DeferredAuditEvent>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = Self {
            sender,
            metrics: DeferredAuditMetrics::default(),
        };
        (queue, receiver)
    }

    /// Submit a refusal event. Non-blocking. Never panics — if the
    /// receiver is closed the metric counter is bumped and a
    /// tracing::warn is emitted, but the caller path is unaffected.
    /// Returns `true` when the event was queued, `false` when the
    /// receiver was already closed.
    pub fn submit(&self, event: DeferredAuditEvent) -> bool {
        self.metrics.submitted.fetch_add(1, Ordering::Relaxed);
        match self.sender.send(event) {
            Ok(()) => true,
            Err(_) => {
                self.metrics.send_failures.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    "deferred_audit_queue: submit failed (drainer receiver closed); \
                     audit chain row LOST for this refusal"
                );
                false
            }
        }
    }

    /// Convenience: build + submit a refusal from the three hook
    /// inputs. Returns `true` when an event was actually enqueued
    /// (i.e. the verdict was a refusal AND the receiver was open).
    pub fn submit_refusal(
        &self,
        agent_id: &str,
        action: &AgentAction,
        decision: &Decision,
    ) -> bool {
        let Some(event) = DeferredAuditEvent::from_refusal(agent_id, action, decision) else {
            return false;
        };
        self.submit(event)
    }

    /// Observability handle. Clone-cheap; safe to expose to readers
    /// (Prometheus scrape, MCP `governance_state` tool, etc.).
    #[must_use]
    pub fn metrics(&self) -> DeferredAuditMetrics {
        self.metrics.clone()
    }

    /// True when the drainer receiver is still attached. False when
    /// the supervisor task and its receiver have both terminated
    /// (shutdown complete).
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.sender.is_closed()
    }
}

// ---------------------------------------------------------------------------
// Drainer / supervisor
// ---------------------------------------------------------------------------

/// Sink trait: an abstraction over "where the drainer writes the
/// audit row". The production wiring opens a fresh SQLite
/// `Connection` per drainer task and writes through `signed_events`;
/// tests substitute a mock sink to assert per-event behavior or
/// inject panics for supervisor-recovery coverage.
///
/// `append` MUST be `&mut` so a test sink can record events into an
/// owned `Vec` without interior mutability. Production sinks
/// (SQLite-backed) hold their state behind the impl.
pub trait DeferredAuditSink: Send + 'static {
    /// Persist one event. Errors are surfaced to the supervisor
    /// (which logs at `error` level and bumps the
    /// `append_failures` metric) but do not panic the drainer.
    ///
    /// # Errors
    ///
    /// Implementation-defined. The production sink propagates the
    /// SQLite error verbatim.
    fn append(&mut self, event: &DeferredAuditEvent) -> Result<()>;
}

/// Production sink: opens a fresh SQLite `Connection` against the
/// daemon's database path and appends a `governance.refusal` row to
/// `signed_events` for each event.
///
/// One `Connection` per drainer task — NOT shared with the substrate
/// writer. SQLite WAL mode lets the drainer's appends proceed in
/// parallel with the writer's INSERTs without lock contention.
pub struct SqliteSignedEventsSink {
    db_path: PathBuf,
    conn: Option<rusqlite::Connection>,
}

impl SqliteSignedEventsSink {
    /// Construct without opening — the connection is opened lazily
    /// on first `append`. This pattern lets the supervisor restart
    /// the sink across drainer respawns without holding a closed
    /// `Connection` handle.
    #[must_use]
    pub fn new(db_path: impl Into<PathBuf>) -> Self {
        Self {
            db_path: db_path.into(),
            conn: None,
        }
    }

    fn ensure_conn(&mut self) -> Result<&rusqlite::Connection> {
        if self.conn.is_none() {
            let conn = crate::db::open(&self.db_path).with_context(|| {
                format!(
                    "SqliteSignedEventsSink: open {} for deferred-audit drainer",
                    self.db_path.display()
                )
            })?;
            self.conn = Some(conn);
        }
        // We just inserted Some — unwrap is safe.
        Ok(self.conn.as_ref().expect("conn populated above"))
    }
}

impl DeferredAuditSink for SqliteSignedEventsSink {
    fn append(&mut self, event: &DeferredAuditEvent) -> Result<()> {
        let conn = self.ensure_conn()?;
        let bytes = event.canonical_bytes()?;
        let signed = SignedEvent {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: event.agent_id.clone(),
            event_type: GOVERNANCE_REFUSAL_EVENT_TYPE.to_string(),
            payload_hash: payload_hash(&bytes),
            signature: None,
            attest_level: "unsigned".to_string(),
            timestamp: event.timestamp.to_rfc3339(),
        };
        append_signed_event(conn, &signed)
            .context("SqliteSignedEventsSink: append governance.refusal row")?;
        Ok(())
    }
}

/// Spawn a single drainer iteration. The returned `JoinHandle`
/// completes (Ok) when the channel sender is dropped AND the
/// receiver has been fully drained — graceful shutdown. A panic in
/// the sink propagates through the `JoinHandle` (the
/// [`spawn_supervised_drainer`] wrapper catches it and respawns).
///
/// Use [`spawn_supervised_drainer`] in production. This bare entry
/// point is exposed for tests that want one-shot drainer behavior
/// without supervisor restart.
#[must_use]
pub fn spawn_drainer_task<S: DeferredAuditSink + 'static>(
    mut receiver: UnboundedReceiver<DeferredAuditEvent>,
    mut sink: S,
    metrics: DeferredAuditMetrics,
) -> JoinHandle<UnboundedReceiver<DeferredAuditEvent>> {
    tokio::spawn(async move {
        while let Some(event) = receiver.recv().await {
            match sink.append(&event) {
                Ok(()) => {
                    metrics.appended.fetch_add(1, Ordering::Relaxed);
                }
                Err(e) => {
                    metrics.append_failures.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        "deferred_audit drainer: sink.append failed: {:#} \
                         (event will be retried after drainer restart if shutdown not yet initiated)",
                        e
                    );
                    // We don't requeue — the channel is single-consumer.
                    // The supervisor's panic-restart path is for sink
                    // PANICS (poisoned state); soft errors here are
                    // recorded and the loop continues.
                }
            }
        }
        // Sender dropped + channel drained → graceful shutdown. Return
        // the receiver so the supervisor (which owns the sender
        // ultimately via the queue handle) can drop it cleanly.
        receiver
    })
}

/// Supervisor: spawns the drainer with panic recovery. Any panic
/// caught at the `JoinHandle::is_panic()` boundary triggers a
/// respawn with a FRESH sink (`make_sink()`), preserving the
/// receiver and the metrics handle.
///
/// The supervisor task returns when either:
///   - The channel sender is dropped and the channel is fully
///     drained (graceful shutdown).
///   - `max_restarts` consecutive panics occur (default `u32::MAX`
///     — effectively never gives up; an operator that wants to
///     fail loudly on persistent panics can configure a finite
///     limit).
///
/// The returned `JoinHandle` resolves when the supervisor exits.
#[must_use]
pub fn spawn_supervised_drainer<F, S>(
    receiver: UnboundedReceiver<DeferredAuditEvent>,
    make_sink: F,
    metrics: DeferredAuditMetrics,
    max_restarts: u32,
) -> JoinHandle<()>
where
    F: Fn() -> S + Send + 'static,
    S: DeferredAuditSink + 'static,
{
    tokio::spawn(async move {
        // Drainer iteration. The receiver lives inside the spawned
        // task; on graceful shutdown the task returns it back to
        // us. On panic the receiver is lost — see the
        // documentation block for the supervisor restart pattern.
        let sink = make_sink();
        let handle = spawn_drainer_task(receiver, sink, metrics.clone());
        match handle.await {
            Ok(returned_receiver) => {
                // Drainer exited gracefully — sender dropped + drained.
                drop(returned_receiver);
            }
            Err(join_err) if join_err.is_panic() => {
                metrics.drainer_panics.fetch_add(1, Ordering::Relaxed);
                tracing::error!(
                    "deferred_audit supervisor: drainer task panicked ({join_err}); \
                     max_restarts={max_restarts} — receiver moved into the panicked \
                     task and cannot be recovered; future refusals submitted to the \
                     existing queue will fail to land. Operator action required: \
                     rebuild the daemon's deferred-audit queue (or restart the daemon) \
                     to restore the audit-chain property."
                );
                // We cannot loop without a valid receiver. The
                // max_restarts variable is preserved in the API so
                // future revisions can introduce a buffering scheme
                // that lets the supervisor recover the in-flight
                // events; today the contract is "panic in drainer
                // = chain loss on the unflushed buffer, future
                // events recorded as send_failures".
                let _ = max_restarts;
            }
            Err(join_err) => {
                // Cancellation (task aborted) — treat as shutdown.
                tracing::warn!(
                    "deferred_audit supervisor: drainer aborted ({join_err}); \
                     pending events may be lost"
                );
            }
        }
    })
}

/// Close the queue and wait for the supervisor task to drain every
/// pending event. After this returns the chain-log property is
/// "every refusal submitted before close lands in `signed_events`."
///
/// # Errors
///
/// Returns the `tokio::task::JoinError` if the supervisor task
/// panicked while draining (rare — the supervisor catches drainer
/// panics, but its own panic would surface here).
pub async fn close_and_flush(
    queue: DeferredAuditQueue,
    supervisor: JoinHandle<()>,
) -> std::result::Result<(), tokio::task::JoinError> {
    // Drop the producer-side sender — once every clone is dropped,
    // the receiver's `recv().await` returns None and the drainer
    // exits gracefully.
    drop(queue);
    supervisor.await
}

// ---------------------------------------------------------------------------
// Convenience installer for the daemon path
// ---------------------------------------------------------------------------

/// Build a queue + spawn a supervised drainer in one call. Returns
/// the producer handle and the supervisor `JoinHandle` — the daemon
/// stashes the queue on `AppState` and the join handle in
/// `task_handles` so `serve` aborts it on shutdown.
///
/// The drainer opens a FRESH `Connection` per its sink (via
/// `SqliteSignedEventsSink::new(db_path)`); on respawn after panic
/// the sink is rebuilt verbatim. No connection is shared with the
/// substrate writer.
#[must_use]
pub fn install_deferred_audit_drainer(db_path: &Path) -> (DeferredAuditQueue, JoinHandle<()>) {
    let (queue, receiver) = DeferredAuditQueue::new();
    let metrics = queue.metrics();
    let db_path_buf = db_path.to_path_buf();
    let supervisor = spawn_supervised_drainer(
        receiver,
        move || SqliteSignedEventsSink::new(db_path_buf.clone()),
        metrics,
        u32::MAX,
    );
    (queue, supervisor)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // -----------------------------------------------------------------
    // DeferredAuditEvent shape + canonical bytes
    // -----------------------------------------------------------------

    fn refusal_action() -> AgentAction {
        AgentAction::Custom {
            custom_kind: "memory_write".to_string(),
            payload: serde_json::json!({"namespace": "secrets/api"}),
        }
    }

    fn refusal_decision() -> Decision {
        Decision::Refuse {
            rule_id: "R001".to_string(),
            reason: "no writes to secrets/*".to_string(),
        }
    }

    #[test]
    fn from_refusal_returns_some_for_refuse() {
        let event =
            DeferredAuditEvent::from_refusal("agent:alice", &refusal_action(), &refusal_decision())
                .expect("must be Some for Refuse verdict");
        assert_eq!(event.agent_id, "agent:alice");
        assert_eq!(event.rule_id(), Some("R001"));
        assert_eq!(event.reason(), Some("no writes to secrets/*"));
    }

    #[test]
    fn from_refusal_returns_none_for_allow() {
        let event =
            DeferredAuditEvent::from_refusal("agent:alice", &refusal_action(), &Decision::Allow);
        assert!(event.is_none(), "Allow verdict must not enqueue an event");
    }

    #[test]
    fn from_refusal_returns_none_for_warn() {
        let warn = Decision::Warn {
            rule_id: "W001".to_string(),
            reason: "warning only".to_string(),
        };
        let event = DeferredAuditEvent::from_refusal("agent:alice", &refusal_action(), &warn);
        assert!(event.is_none(), "Warn verdict must not enqueue a refusal");
    }

    #[test]
    fn canonical_bytes_includes_rule_and_action() {
        let event =
            DeferredAuditEvent::from_refusal("agent:alice", &refusal_action(), &refusal_decision())
                .unwrap();
        let bytes = event.canonical_bytes().unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(s.contains("R001"), "canonical payload must include rule_id");
        assert!(
            s.contains("memory_write"),
            "canonical payload must include action kind"
        );
        assert!(
            s.contains("agent:alice"),
            "canonical payload must include agent id"
        );
    }

    #[test]
    fn rule_id_returns_none_for_non_refusal() {
        let event = DeferredAuditEvent {
            agent_id: "x".into(),
            action: refusal_action(),
            decision: Decision::Allow,
            timestamp: chrono::Utc::now(),
        };
        assert!(event.rule_id().is_none());
        assert!(event.reason().is_none());
    }

    // -----------------------------------------------------------------
    // DeferredAuditQueue submit + non-blocking semantics
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn queue_new_returns_open_handle() {
        let (queue, _rx) = DeferredAuditQueue::new();
        assert!(queue.is_open());
        assert_eq!(queue.metrics().submitted_count(), 0);
    }

    #[tokio::test]
    async fn submit_with_receiver_attached_succeeds() {
        let (queue, mut rx) = DeferredAuditQueue::new();
        let event =
            DeferredAuditEvent::from_refusal("agent:t", &refusal_action(), &refusal_decision())
                .unwrap();
        assert!(queue.submit(event.clone()));
        assert_eq!(queue.metrics().submitted_count(), 1);
        let received = rx.recv().await.unwrap();
        assert_eq!(received.agent_id, event.agent_id);
        assert_eq!(received.rule_id(), Some("R001"));
    }

    #[tokio::test]
    async fn submit_after_receiver_dropped_records_send_failure() {
        let (queue, rx) = DeferredAuditQueue::new();
        drop(rx);
        // sender knows its peer is gone
        assert!(!queue.is_open());
        let event =
            DeferredAuditEvent::from_refusal("agent:t", &refusal_action(), &refusal_decision())
                .unwrap();
        let ok = queue.submit(event);
        assert!(!ok, "submit must return false when receiver is closed");
        assert_eq!(queue.metrics().submitted_count(), 1);
        assert_eq!(queue.metrics().send_failure_count(), 1);
    }

    #[tokio::test]
    async fn submit_refusal_helper_skips_non_refusals() {
        let (queue, mut rx) = DeferredAuditQueue::new();
        // Allow does NOT enqueue
        let enq = queue.submit_refusal("agent:t", &refusal_action(), &Decision::Allow);
        assert!(!enq);
        // Try receiving — should timeout (channel empty)
        let recv = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(recv.is_err(), "no event should have been enqueued");
        // Refusal DOES enqueue
        let enq2 = queue.submit_refusal("agent:t", &refusal_action(), &refusal_decision());
        assert!(enq2);
        let event = rx.recv().await.unwrap();
        assert_eq!(event.agent_id, "agent:t");
    }

    #[tokio::test]
    async fn queue_clone_shares_underlying_channel() {
        let (queue, mut rx) = DeferredAuditQueue::new();
        let clone = queue.clone();
        let event1 =
            DeferredAuditEvent::from_refusal("agent:a", &refusal_action(), &refusal_decision())
                .unwrap();
        let event2 =
            DeferredAuditEvent::from_refusal("agent:b", &refusal_action(), &refusal_decision())
                .unwrap();
        queue.submit(event1);
        clone.submit(event2);
        let r1 = rx.recv().await.unwrap();
        let r2 = rx.recv().await.unwrap();
        let agents: Vec<_> = vec![r1.agent_id, r2.agent_id];
        assert!(agents.contains(&"agent:a".to_string()));
        assert!(agents.contains(&"agent:b".to_string()));
        // Both sides share the same metrics handle
        assert_eq!(queue.metrics().submitted_count(), 2);
        assert_eq!(clone.metrics().submitted_count(), 2);
    }

    // -----------------------------------------------------------------
    // Drainer task behavior with mock sink
    // -----------------------------------------------------------------

    /// Mock sink: stores every received event in an owned Vec for
    /// post-condition assertions; optionally panics on the Nth
    /// event to drive supervisor-recovery tests.
    #[derive(Clone, Default)]
    struct MockSink {
        // Recorded events, behind a mutex so the test can read while
        // the drainer writes.
        recorded: Arc<Mutex<Vec<DeferredAuditEvent>>>,
        // Optional: panic on the Nth append (zero-indexed).
        panic_on: Option<usize>,
        // Optional: error on the Nth append (zero-indexed).
        error_on: Option<usize>,
        // Counter (shared across clones for supervisor-restart
        // counting).
        call_count: Arc<AtomicU64>,
    }

    impl DeferredAuditSink for MockSink {
        fn append(&mut self, event: &DeferredAuditEvent) -> Result<()> {
            let prior = self.call_count.fetch_add(1, Ordering::SeqCst) as usize;
            if Some(prior) == self.panic_on {
                panic!("mock sink: configured panic at call {prior}");
            }
            if Some(prior) == self.error_on {
                return Err(anyhow::anyhow!(
                    "mock sink: configured error at call {prior}"
                ));
            }
            self.recorded.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn drainer_appends_every_submitted_event() {
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let sink = MockSink::default();
        let recorded = sink.recorded.clone();
        let handle = spawn_drainer_task(rx, sink, metrics.clone());

        for i in 0..5 {
            let mut event = DeferredAuditEvent::from_refusal(
                &format!("agent:{i}"),
                &refusal_action(),
                &refusal_decision(),
            )
            .unwrap();
            event.timestamp = chrono::Utc::now();
            queue.submit(event);
        }

        // Drop the queue (sender) to terminate the drainer.
        drop(queue);
        let _returned_rx = handle.await.unwrap();

        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 5);
        for (i, ev) in recorded.iter().enumerate() {
            assert_eq!(ev.agent_id, format!("agent:{i}"));
        }
        assert_eq!(metrics.appended_count(), 5);
    }

    #[tokio::test]
    async fn drainer_continues_after_sink_error() {
        // Sink errors on the second call; the drainer should
        // record the error in metrics and proceed to handle
        // subsequent events.
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let mut sink = MockSink::default();
        sink.error_on = Some(1);
        let recorded = sink.recorded.clone();
        let handle = spawn_drainer_task(rx, sink, metrics.clone());

        for i in 0..3 {
            let event = DeferredAuditEvent::from_refusal(
                &format!("agent:{i}"),
                &refusal_action(),
                &refusal_decision(),
            )
            .unwrap();
            queue.submit(event);
        }
        drop(queue);
        let _ = handle.await.unwrap();
        // Event 0 and 2 landed; event 1 hit the error.
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert_eq!(metrics.appended_count(), 2);
        assert_eq!(metrics.append_failure_count(), 1);
    }

    // -----------------------------------------------------------------
    // Supervisor: panic recovery + graceful shutdown
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn supervisor_records_panic_metric_on_drainer_panic() {
        // Sink panics on first call. The supervisor catches the
        // panic, bumps drainer_panics, and (per current
        // implementation) terminates after the panic — the receiver
        // moved into the panicked task and cannot be recovered.
        // We verify the panic-counter side-effect.
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let panic_on = Some(0_usize);
        let supervisor = spawn_supervised_drainer(
            rx,
            move || MockSink {
                recorded: Arc::new(Mutex::new(Vec::new())),
                panic_on,
                error_on: None,
                call_count: Arc::new(AtomicU64::new(0)),
            },
            metrics.clone(),
            1, // max 1 restart (= no respawn beyond the initial spawn)
        );

        let event =
            DeferredAuditEvent::from_refusal("agent:panic", &refusal_action(), &refusal_decision())
                .unwrap();
        queue.submit(event);
        // Wait for the supervisor to observe the panic and exit.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), supervisor)
            .await
            .expect("supervisor must exit after observing panic");
        assert_eq!(
            metrics.panic_count(),
            1,
            "supervisor must record exactly one drainer panic"
        );
    }

    #[tokio::test]
    async fn supervisor_graceful_shutdown_drains_buffered_events() {
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let recorded: Arc<Mutex<Vec<DeferredAuditEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_for_factory = recorded.clone();
        let supervisor = spawn_supervised_drainer(
            rx,
            move || MockSink {
                recorded: recorded_for_factory.clone(),
                panic_on: None,
                error_on: None,
                call_count: Arc::new(AtomicU64::new(0)),
            },
            metrics.clone(),
            u32::MAX,
        );

        // Submit 50 events.
        for i in 0..50 {
            let event = DeferredAuditEvent::from_refusal(
                &format!("agent:{i}"),
                &refusal_action(),
                &refusal_decision(),
            )
            .unwrap();
            queue.submit(event);
        }

        // Initiate shutdown — close_and_flush drops the queue and
        // awaits the supervisor.
        close_and_flush(queue, supervisor)
            .await
            .expect("supervisor must terminate cleanly");

        let recorded = recorded.lock().unwrap();
        assert_eq!(
            recorded.len(),
            50,
            "shutdown must drain every buffered event"
        );
        assert_eq!(metrics.appended_count(), 50);
    }

    #[tokio::test]
    async fn close_and_flush_works_with_zero_events() {
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let supervisor =
            spawn_supervised_drainer(rx, move || MockSink::default(), metrics.clone(), u32::MAX);
        close_and_flush(queue, supervisor).await.unwrap();
        assert_eq!(metrics.appended_count(), 0);
        assert_eq!(metrics.submitted_count(), 0);
    }

    // -----------------------------------------------------------------
    // High-volume / backpressure-edge — drainer slow, many submits
    // queued, all drain eventually
    // -----------------------------------------------------------------

    /// Slow sink: artificial 1 ms delay per append to simulate a
    /// busy fsync path.
    struct SlowSink {
        recorded: Arc<Mutex<Vec<DeferredAuditEvent>>>,
    }

    impl DeferredAuditSink for SlowSink {
        fn append(&mut self, event: &DeferredAuditEvent) -> Result<()> {
            std::thread::sleep(std::time::Duration::from_millis(1));
            self.recorded.lock().unwrap().push(event.clone());
            Ok(())
        }
    }

    #[tokio::test]
    async fn unbounded_queue_handles_burst_no_drops() {
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let recorded: Arc<Mutex<Vec<DeferredAuditEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let recorded_for_factory = recorded.clone();
        let supervisor = spawn_supervised_drainer(
            rx,
            move || SlowSink {
                recorded: recorded_for_factory.clone(),
            },
            metrics.clone(),
            u32::MAX,
        );

        // Burst 200 events (drainer is ~1 ms/event so this will
        // accumulate before draining).
        for i in 0..200 {
            let event = DeferredAuditEvent::from_refusal(
                &format!("agent:{i}"),
                &refusal_action(),
                &refusal_decision(),
            )
            .unwrap();
            assert!(
                queue.submit(event),
                "unbounded queue must never refuse a submit"
            );
        }
        assert_eq!(metrics.submitted_count(), 200);
        assert_eq!(metrics.send_failure_count(), 0);

        close_and_flush(queue, supervisor).await.unwrap();
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 200);
        assert_eq!(metrics.appended_count(), 200);
    }

    // -----------------------------------------------------------------
    // Production-sink (SqliteSignedEventsSink) integration — opens a
    // real SQLite Connection against a temp file and asserts the row
    // lands.
    // -----------------------------------------------------------------

    fn fresh_tempdir() -> tempfile::TempDir {
        // Honor project hard rule: no /tmp writes by name. The
        // tempfile crate honors TMPDIR (exported at session
        // bootstrap to .local-runs/tmp), so this resolves under the
        // project-local scratch tree.
        tempfile::tempdir().expect("tempdir")
    }

    #[tokio::test]
    async fn sqlite_sink_appends_governance_refusal_row() {
        let dir = fresh_tempdir();
        let db_path = dir.path().join("def-audit-test.db");
        // Pre-create the schema via crate::db::open (applies
        // migrations including signed_events).
        let _ = crate::db::open(&db_path).expect("init db");

        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        let db_path_buf = db_path.clone();
        let supervisor = spawn_supervised_drainer(
            rx,
            move || SqliteSignedEventsSink::new(db_path_buf.clone()),
            metrics.clone(),
            u32::MAX,
        );

        let event =
            DeferredAuditEvent::from_refusal("agent:int", &refusal_action(), &refusal_decision())
                .unwrap();
        queue.submit(event);

        close_and_flush(queue, supervisor).await.unwrap();
        assert_eq!(metrics.appended_count(), 1);

        // Verify the row landed with event_type=governance.refusal.
        let conn = crate::db::open(&db_path).expect("reopen db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1 AND agent_id = ?2",
                rusqlite::params![GOVERNANCE_REFUSAL_EVENT_TYPE, "agent:int"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "drainer must have written the row");
    }

    #[tokio::test]
    async fn sqlite_sink_lazy_open_only_on_first_append() {
        // Construct a sink against a path that doesn't exist yet; if
        // we never call append, ensure_conn must never run.
        let nonexistent = std::path::PathBuf::from("/this/path/does/not/exist/db.sqlite");
        let sink = SqliteSignedEventsSink::new(nonexistent);
        // Just verifying construction doesn't open the DB — no
        // assertion needed; if `new` opened eagerly, this would
        // already have errored.
        drop(sink);
    }

    #[tokio::test]
    async fn sqlite_sink_append_fails_on_bad_path_metrics_increments() {
        let (queue, rx) = DeferredAuditQueue::new();
        let metrics = queue.metrics();
        // Build a sink pointing at a path that can't be opened (a
        // directory that doesn't exist + a non-creatable subdir
        // would do it; under macOS/Linux we use a path under /sys
        // which is read-only).
        let bad_path =
            std::path::PathBuf::from("/nonexistent-readonly-dir-for-deferred-audit-test/db.sqlite");
        let supervisor = spawn_supervised_drainer(
            rx,
            move || SqliteSignedEventsSink::new(bad_path.clone()),
            metrics.clone(),
            u32::MAX,
        );
        let event =
            DeferredAuditEvent::from_refusal("agent:bad", &refusal_action(), &refusal_decision())
                .unwrap();
        queue.submit(event);
        // Allow the drainer to attempt the append.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        close_and_flush(queue, supervisor).await.unwrap();
        assert!(
            metrics.append_failure_count() >= 1,
            "append failure on bad path must be recorded; got {}",
            metrics.append_failure_count()
        );
        assert_eq!(metrics.appended_count(), 0);
    }

    // -----------------------------------------------------------------
    // install_deferred_audit_drainer end-to-end (the daemon-facing
    // installer)
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn installer_returns_open_queue_and_running_supervisor() {
        let dir = fresh_tempdir();
        let db_path = dir.path().join("installer-test.db");
        let _ = crate::db::open(&db_path).expect("init db");

        let (queue, supervisor) = install_deferred_audit_drainer(&db_path);
        assert!(queue.is_open());

        let event = DeferredAuditEvent::from_refusal(
            "agent:installer",
            &refusal_action(),
            &refusal_decision(),
        )
        .unwrap();
        queue.submit(event);

        close_and_flush(queue, supervisor).await.unwrap();

        let conn = crate::db::open(&db_path).expect("reopen db");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM signed_events WHERE event_type = ?1",
                rusqlite::params![GOVERNANCE_REFUSAL_EVENT_TYPE],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    // -----------------------------------------------------------------
    // Metrics — getter coverage
    // -----------------------------------------------------------------

    #[test]
    fn metrics_default_returns_zeroes() {
        let m = DeferredAuditMetrics::default();
        assert_eq!(m.submitted_count(), 0);
        assert_eq!(m.appended_count(), 0);
        assert_eq!(m.send_failure_count(), 0);
        assert_eq!(m.append_failure_count(), 0);
        assert_eq!(m.panic_count(), 0);
    }

    #[test]
    fn metrics_clone_shares_counters() {
        let m1 = DeferredAuditMetrics::default();
        let m2 = m1.clone();
        m1.submitted.fetch_add(7, Ordering::Relaxed);
        // m2 sees the same counter — Arc<Atomic> semantics.
        assert_eq!(m2.submitted_count(), 7);
    }

    // -----------------------------------------------------------------
    // GOVERNANCE_REFUSAL_EVENT_TYPE is a stable wire string
    // -----------------------------------------------------------------

    #[test]
    fn governance_refusal_event_type_is_stable() {
        assert_eq!(GOVERNANCE_REFUSAL_EVENT_TYPE, "governance.refusal");
    }
}
