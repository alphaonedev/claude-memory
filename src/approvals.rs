// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 K10 — Approval API (HTTP + SSE + MCP).
//!
//! When the governance gate returns `Pending`, an operator must
//! eventually decide. v0.7.0 surfaces three transports for that
//! decision:
//!
//! 1. **HTTP** — `POST /api/v1/approvals/{pending_id}` with the body
//!    `{"decision":"approve|deny","remember":"once|session|forever"}`.
//!    Gated behind the K7 `[hooks.subscription] hmac_secret` server-wide
//!    HMAC: requests without a valid `X-AI-Memory-Signature: sha256=…`
//!    header are rejected `401`.
//! 2. **SSE** — `GET /api/v1/approvals/stream` server-sent events.
//!    Subscribers receive `approval_requested` (one per new
//!    `pending_actions` row) and `approval_decided` (one per
//!    approve/deny outcome) frames, fanned out through a process-wide
//!    `tokio::sync::broadcast` channel so multiple watchers can attach
//!    concurrently without contention on the DB lock.
//! 3. **MCP** — the existing `memory_pending_approve` /
//!    `memory_pending_reject` tools gain an optional `remember`
//!    property. The K10 contract preserves the pre-K10 schema (no new
//!    tools, no removed properties) — so existing callers keep working
//!    unchanged and only opt into `remember` when they want
//!    forever-persisted permission rules.
//!
//! When `remember = "forever"`, K10 stamps a synthetic
//! [`SyntheticPermissionRule`] into the process-wide registry so the
//! same `(action, namespace, agent_id)` tuple auto-decides next time.
//! K9 (the unified permission pipeline) will consult the registry from
//! its rule-evaluation path; until K9 lands on this branch, the
//! registry exists as an isolated K10-internal store that the K10 test
//! suite can introspect to pin the contract.

use std::sync::OnceLock;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

/// Capacity of the process-wide approval broadcast channel.
///
/// Sized to absorb a brief spike of `approval_requested` /
/// `approval_decided` events without forcing a slow SSE subscriber to
/// drop frames. SSE consumers see [`broadcast::error::RecvError::Lagged`]
/// when this is exceeded; the [`approvals_sse`](crate::handlers::approvals_sse)
/// handler turns that into a `lagged` SSE event so clients can re-sync
/// via `GET /api/v1/pending`.
pub const APPROVAL_BROADCAST_CAPACITY: usize = 1024;

/// Decision an operator submits via the K10 transports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Approve,
    Deny,
}

/// How long a `remember` choice persists.
///
/// - `Once` — just this decision; no rule recorded.
/// - `Session` — recorded in-memory; cleared on restart.
/// - `Forever` — recorded in-memory AND queued for persistence to the
///   live `config.toml` `[[permissions.rules]]` table on the next
///   config write. (The actual disk write is owned by the K9 rule
///   loader; K10's contract is to populate the registry that K9
///   consults.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Remember {
    Once,
    Session,
    Forever,
}

/// One row in the K10 synthetic-permission-rule registry.
///
/// Mirrors the shape K9's `[[permissions.rules]]` table will use
/// once K9 lands on the same branch — that way K9's loader can
/// promote these in-memory rows into config-file rows without a
/// schema translation step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyntheticPermissionRule {
    /// `pending_actions.action_type` — `"store"`, `"delete"`, or `"promote"`.
    pub action_type: String,
    /// `pending_actions.namespace` — the namespace the original gated
    /// action targeted.
    pub namespace: String,
    /// `pending_actions.requested_by` — the agent the rule auto-decides
    /// for. `None` means "any agent in this namespace" (rare, but the
    /// K10 contract reserves the slot for fleet-wide rules).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// `"approve"` or `"deny"` — the auto-decision the gate should
    /// return next time it sees a matching tuple.
    pub decision: String,
    /// RFC3339 timestamp the rule was recorded. Surfaced in audit
    /// trails and (eventually) in K9's rule-summary doctor surface.
    pub recorded_at: String,
}

/// Process-wide registry of `remember=forever` rules. Populated by
/// the K10 transports; read by K9's rule resolver (when K9 lands).
static SYNTHETIC_RULES: RwLock<Vec<SyntheticPermissionRule>> = RwLock::new(Vec::new());

/// Append a synthetic rule to the registry.
///
/// Idempotent on the `(action_type, namespace, agent_id, decision)`
/// tuple — calling twice with the same tuple is a no-op (the recorded
/// timestamp from the first insert wins). Lock poisoning is treated as
/// fatal-but-recoverable: we drop the poisoned guard and proceed
/// against the inner data, mirroring the K3 `lock_permissions_mode_for_test`
/// posture.
pub fn record_synthetic_rule(rule: SyntheticPermissionRule) {
    let mut guard = SYNTHETIC_RULES
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let already = guard.iter().any(|r| {
        r.action_type == rule.action_type
            && r.namespace == rule.namespace
            && r.agent_id == rule.agent_id
            && r.decision == rule.decision
    });
    if !already {
        guard.push(rule);
    }
}

/// Snapshot the registry. Returns a clone so callers can release the
/// read lock immediately.
#[must_use]
pub fn list_synthetic_rules() -> Vec<SyntheticPermissionRule> {
    SYNTHETIC_RULES
        .read()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// Test-only: clear the registry. Production code never resets the
/// registry mid-process; tests use this to assert against a clean slate.
#[doc(hidden)]
pub fn clear_synthetic_rules_for_test() {
    if let Ok(mut g) = SYNTHETIC_RULES.write() {
        g.clear();
    }
}

/// One frame on the SSE stream.
///
/// Two variants today:
///   - `ApprovalRequested` — fired when a `pending_actions` row is
///     inserted (governance gate returned `Pending`).
///   - `ApprovalDecided` — fired when an approve/reject decision is
///     finalised (any of the three K10 transports).
///
/// Both carry the pending-action id so subscribers can round-trip back
/// through `GET /api/v1/pending/{id}` for the full row payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum ApprovalEvent {
    ApprovalRequested {
        pending_id: String,
        action_type: String,
        namespace: String,
        requested_by: String,
        requested_at: String,
    },
    ApprovalDecided {
        pending_id: String,
        decision: String,
        decided_by: String,
        remember: String,
        /// Originating namespace of the pending row this decision
        /// targets. Required by the K10 SSE filter (review #628
        /// blocker C2): without it the receive-side filter cannot
        /// scope the event to the right tenant.
        #[serde(default)]
        namespace: String,
        /// Original requester for the pending row this decision
        /// targets. Same rationale as `namespace` — the decision
        /// frame is delivered to the original requester even if a
        /// different operator pressed the approve button.
        #[serde(default)]
        requested_by: String,
    },
}

impl ApprovalEvent {
    /// Tenant agent the event belongs to — `requested_by` for both
    /// variants. Used by the SSE handler to scope broadcasts to the
    /// originating agent (review #628 blocker C2).
    #[must_use]
    pub fn tenant_agent_id(&self) -> &str {
        match self {
            ApprovalEvent::ApprovalRequested { requested_by, .. }
            | ApprovalEvent::ApprovalDecided { requested_by, .. } => requested_by.as_str(),
        }
    }

    /// Namespace the event belongs to. Used by the SSE handler in
    /// concert with K9's permission rules to decide whether a
    /// subscriber may see a cross-agent event.
    #[must_use]
    pub fn tenant_namespace(&self) -> &str {
        match self {
            ApprovalEvent::ApprovalRequested { namespace, .. }
            | ApprovalEvent::ApprovalDecided { namespace, .. } => namespace.as_str(),
        }
    }
}

/// Process-wide broadcast channel for [`ApprovalEvent`]. Lazily
/// initialised on first subscribe / publish — the server's HTTP layer
/// touches it from `handlers::approvals_sse` and the publish side fires
/// from `handlers::approve_via_approval_api`,
/// `subscriptions::dispatch_approval_requested`, and the MCP
/// approve/reject handlers.
static APPROVAL_BUS: OnceLock<broadcast::Sender<ApprovalEvent>> = OnceLock::new();

fn bus() -> &'static broadcast::Sender<ApprovalEvent> {
    APPROVAL_BUS.get_or_init(|| {
        let (tx, _rx) = broadcast::channel(APPROVAL_BROADCAST_CAPACITY);
        tx
    })
}

/// Publish an [`ApprovalEvent`] to all SSE subscribers.
///
/// No subscribers → swallowed silently (the `broadcast::Sender::send`
/// `Err(SendError(_))` branch is the documented "no receivers" outcome
/// and is never an error in this codebase: SSE is best-effort and we
/// must not fail the underlying approve/reject path on a missing
/// subscriber).
pub fn publish(event: ApprovalEvent) {
    let _ = bus().send(event);
}

/// Subscribe to the process-wide approval bus. Returns a fresh
/// [`broadcast::Receiver`] that will see every event published AFTER
/// this call (broadcast channels do not replay history — that's what
/// `GET /api/v1/pending` is for).
#[must_use]
pub fn subscribe() -> broadcast::Receiver<ApprovalEvent> {
    bus().subscribe()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialise the unit tests that mutate the global registry —
    /// `cargo test` runs tests in parallel by default and the
    /// `SYNTHETIC_RULES` static is shared across them.
    fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::Mutex;
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn record_and_list_round_trip() {
        let _g = registry_lock();
        clear_synthetic_rules_for_test();
        let rule = SyntheticPermissionRule {
            action_type: "store".into(),
            namespace: "scratch".into(),
            agent_id: Some("alice".into()),
            decision: "approve".into(),
            recorded_at: "2026-05-05T00:00:00Z".into(),
        };
        record_synthetic_rule(rule.clone());
        let snap = list_synthetic_rules();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0], rule);
    }

    #[test]
    fn record_synthetic_rule_is_idempotent() {
        let _g = registry_lock();
        clear_synthetic_rules_for_test();
        let rule = SyntheticPermissionRule {
            action_type: "delete".into(),
            namespace: "ns".into(),
            agent_id: Some("bob".into()),
            decision: "deny".into(),
            recorded_at: "2026-05-05T00:00:00Z".into(),
        };
        record_synthetic_rule(rule.clone());
        // Second call with a later timestamp must not double the row.
        let mut later = rule.clone();
        later.recorded_at = "2099-01-01T00:00:00Z".into();
        record_synthetic_rule(later);
        let snap = list_synthetic_rules();
        assert_eq!(snap.len(), 1);
        // First-writer-wins on the timestamp.
        assert_eq!(snap[0].recorded_at, "2026-05-05T00:00:00Z");
    }

    #[tokio::test]
    async fn publish_and_subscribe_round_trip() {
        let mut rx = subscribe();
        let evt = ApprovalEvent::ApprovalRequested {
            pending_id: "pa-1".into(),
            action_type: "store".into(),
            namespace: "scratch".into(),
            requested_by: "alice".into(),
            requested_at: "2026-05-05T00:00:00Z".into(),
        };
        publish(evt.clone());
        let received = rx.recv().await.expect("recv");
        match received {
            ApprovalEvent::ApprovalRequested { pending_id, .. } => assert_eq!(pending_id, "pa-1"),
            _ => panic!("wrong variant"),
        }
    }
}
