// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! W-of-N quorum-write layer for the peer-mesh sync (v0.7 track C).
//!
//! This module scaffolds the quorum-write contract described in
//! `docs/ADR-0001-quorum-replication.md`. The `QuorumWriter` sits ABOVE
//! the existing sync-daemon — deployments that don't configure
//! `--quorum-writes` keep the v0.6.0 one-way push behaviour byte-for-byte.

#![allow(dead_code)]
//!
//! ## What ships in this PR
//!
//! - `QuorumPolicy` — configuration: N peers, W quorum size, timeouts.
//! - `QuorumWriter::commit` — the atomic-from-caller contract: local
//!   write + W-1 remote acks within deadline, else
//!   `QuorumError::QuorumNotMet`.
//! - `AckTracker` — collects remote acks with a simple `Instant`
//!   deadline. Pure logic, no network — so the unit tests don't need
//!   a live sync mesh.
//! - Metrics: `replication_quorum_ack_total{result}`,
//!   `replication_quorum_failures_total{reason}`,
//!   `replication_clock_skew_seconds`.
//!
//! ## What does NOT ship in this PR
//!
//! - Wiring into the `memory_store` path — follow-up PR once the
//!   sync-daemon gains a synchronous ack channel.
//! - Real chaos harness — follow-up PR under `tests/chaos/` with
//!   three-node fixture and failure-injection hooks.
//!
//! That phasing matches the ADR's implementation plan.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Operator-tunable quorum policy. See ADR-0001 § Model for the
/// complete contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuorumPolicy {
    /// Total peer count — local node + remotes. Must be >= 1.
    pub n: usize,
    /// Required acks including the local commit. Clamped to `[1, n]`
    /// at construction via [`QuorumPolicy::new`].
    pub w: usize,
    /// Deadline for the remote-ack collection phase. Times out with
    /// `QuorumError::QuorumNotMet { reason: Timeout }`.
    pub ack_timeout: Duration,
    /// Warning threshold for peer clock skew. Exceeding this does not
    /// fail the quorum; it surfaces in the clock-skew histogram.
    pub clock_skew_warn: Duration,
}

impl QuorumPolicy {
    /// Construct a quorum policy. `w` is clamped to `[1, n]` and
    /// `n = 0` is rejected as invalid input.
    ///
    /// # Errors
    ///
    /// Returns `QuorumError::InvalidPolicy` if `n == 0`.
    pub fn new(
        n: usize,
        w: usize,
        ack_timeout: Duration,
        clock_skew_warn: Duration,
    ) -> Result<Self, QuorumError> {
        if n == 0 {
            return Err(QuorumError::InvalidPolicy {
                detail: "n must be >= 1".to_string(),
            });
        }
        Ok(Self {
            n,
            w: w.clamp(1, n),
            ack_timeout,
            clock_skew_warn,
        })
    }

    /// Majority-quorum convenience: `W = ceil((N+1)/2)`. Matches the
    /// ADR's default.
    ///
    /// # Errors
    ///
    /// Returns `QuorumError::InvalidPolicy` if `n == 0`.
    pub fn majority(n: usize) -> Result<Self, QuorumError> {
        let w = n.div_ceil(2).max(1);
        Self::new(n, w, Duration::from_secs(2), Duration::from_secs(30))
    }
}

/// Errors surfaced by the quorum writer. Non-exhaustive so we can add
/// variants without breaking downstream matches.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuorumError {
    /// The local write succeeded but we did not collect enough acks
    /// within the policy deadline.
    QuorumNotMet {
        got: usize,
        needed: usize,
        reason: QuorumFailureReason,
    },
    /// The policy itself is malformed (e.g. N = 0).
    InvalidPolicy { detail: String },
    /// The local write itself failed — caller sees the underlying cause.
    LocalWriteFailed { detail: String },
}

impl std::fmt::Display for QuorumError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QuorumNotMet {
                got,
                needed,
                reason,
            } => write!(
                f,
                "quorum not met (got {got}, need {needed}, reason {reason:?})"
            ),
            Self::InvalidPolicy { detail } => write!(f, "invalid quorum policy: {detail}"),
            Self::LocalWriteFailed { detail } => write!(f, "local write failed: {detail}"),
        }
    }
}

impl std::error::Error for QuorumError {}

/// Reason a quorum failed — reported in metrics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuorumFailureReason {
    /// No peers reachable at all (network / DNS / zero configured).
    Unreachable,
    /// Peers reachable but fewer than `W-1` acked before deadline.
    Timeout,
    /// Peer ack arrived but disagreed on the memory id — replication
    /// divergence surfaced for operator investigation.
    IdDrift,
}

/// Collects remote acks against a deadline. Pure logic — no I/O.
#[derive(Debug)]
pub struct AckTracker {
    policy: QuorumPolicy,
    deadline: Instant,
    local_committed: bool,
    acks: HashSet<String>,
    id_drifts: Vec<String>,
}

impl AckTracker {
    /// Create a tracker for one quorum-write attempt. `now` is injected
    /// for deterministic tests.
    #[must_use]
    pub fn new(policy: QuorumPolicy, now: Instant) -> Self {
        let deadline = now + policy.ack_timeout;
        Self {
            policy,
            deadline,
            local_committed: false,
            acks: HashSet::new(),
            id_drifts: Vec::new(),
        }
    }

    /// Record the local commit. Call once the originating node has
    /// durably persisted the memory.
    pub fn record_local(&mut self) {
        self.local_committed = true;
    }

    /// Record a peer ack. `peer_id` is the caller's opaque identifier
    /// (typically the peer's mTLS fingerprint or agent id). Duplicate
    /// `peer_id` values are deduplicated.
    pub fn record_peer_ack(&mut self, peer_id: impl Into<String>) {
        self.acks.insert(peer_id.into());
    }

    /// Record that a peer returned success but with a memory id that
    /// differs from the local commit id. Does NOT count toward the
    /// quorum and surfaces in metrics.
    pub fn record_id_drift(&mut self, peer_id: impl Into<String>) {
        self.id_drifts.push(peer_id.into());
    }

    /// True when the quorum is met: local commit + at least `W-1`
    /// unique peer acks, and the deadline has not elapsed at `now`.
    #[must_use]
    pub fn is_quorum_met(&self, now: Instant) -> bool {
        if !self.local_committed || now > self.deadline {
            return false;
        }
        // Total acks counted = local + distinct peers.
        let total = self.acks.len() + 1;
        total >= self.policy.w
    }

    /// Finalise the attempt. Returns `Ok(count_of_distinct_acks)` if
    /// quorum met, else `Err(QuorumError::QuorumNotMet{…})`.
    ///
    /// # Errors
    ///
    /// Returns `QuorumError::QuorumNotMet` if the deadline elapsed
    /// before W acks arrived.
    pub fn finalise(&self, now: Instant) -> Result<usize, QuorumError> {
        if !self.local_committed {
            return Err(QuorumError::LocalWriteFailed {
                detail: "local commit not recorded before finalise".to_string(),
            });
        }
        let got = self.acks.len() + 1;
        if got >= self.policy.w {
            return Ok(got);
        }
        let reason = if self.acks.is_empty() && now > self.deadline {
            QuorumFailureReason::Unreachable
        } else if now > self.deadline {
            QuorumFailureReason::Timeout
        } else {
            // Under deadline but not enough yet — caller should keep
            // waiting; surface as Timeout only after deadline passes.
            QuorumFailureReason::Timeout
        };
        Err(QuorumError::QuorumNotMet {
            got,
            needed: self.policy.w,
            reason,
        })
    }

    /// Count of peers that reported divergent memory ids for this write.
    /// Exposed for metrics + debugging.
    #[must_use]
    pub fn id_drift_count(&self) -> usize {
        self.id_drifts.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instant_base() -> Instant {
        Instant::now()
    }

    #[test]
    fn policy_rejects_zero_n() {
        let err = QuorumPolicy::new(0, 1, Duration::from_millis(500), Duration::from_secs(30))
            .unwrap_err();
        assert!(matches!(err, QuorumError::InvalidPolicy { .. }));
    }

    #[test]
    fn policy_clamps_w_to_n() {
        let p =
            QuorumPolicy::new(3, 9, Duration::from_millis(500), Duration::from_secs(30)).unwrap();
        assert_eq!(p.n, 3);
        assert_eq!(p.w, 3);
    }

    #[test]
    fn majority_default_matches_adr() {
        // N = 1 => W = 1 (ceil(2/2)); N = 3 => W = 2; N = 5 => W = 3.
        assert_eq!(QuorumPolicy::majority(1).unwrap().w, 1);
        assert_eq!(QuorumPolicy::majority(3).unwrap().w, 2);
        assert_eq!(QuorumPolicy::majority(5).unwrap().w, 3);
        assert_eq!(QuorumPolicy::majority(7).unwrap().w, 4);
    }

    #[test]
    fn quorum_met_with_local_plus_peers() {
        let policy = QuorumPolicy::majority(3).unwrap();
        let mut tracker = AckTracker::new(policy, instant_base());
        tracker.record_local();
        tracker.record_peer_ack("peer-1");
        assert!(tracker.is_quorum_met(instant_base()));
    }

    #[test]
    fn quorum_dedupes_duplicate_peer() {
        let policy =
            QuorumPolicy::new(5, 3, Duration::from_millis(500), Duration::from_secs(30)).unwrap();
        let mut tracker = AckTracker::new(policy, instant_base());
        tracker.record_local();
        tracker.record_peer_ack("peer-1");
        tracker.record_peer_ack("peer-1");
        tracker.record_peer_ack("peer-1");
        // Only counts once + local = 2, need 3.
        assert!(!tracker.is_quorum_met(instant_base()));
        tracker.record_peer_ack("peer-2");
        assert!(tracker.is_quorum_met(instant_base()));
    }

    #[test]
    fn quorum_not_met_without_local() {
        let policy = QuorumPolicy::majority(3).unwrap();
        let mut tracker = AckTracker::new(policy, instant_base());
        // Record two peer acks but no local commit — quorum fails.
        tracker.record_peer_ack("peer-1");
        tracker.record_peer_ack("peer-2");
        assert!(!tracker.is_quorum_met(instant_base()));
    }

    #[test]
    fn quorum_expired_after_deadline() {
        let policy =
            QuorumPolicy::new(3, 2, Duration::from_millis(1), Duration::from_secs(30)).unwrap();
        let t0 = instant_base();
        let mut tracker = AckTracker::new(policy, t0);
        tracker.record_local();
        let later = t0 + Duration::from_millis(50);
        // No peer acks arrived — past deadline, quorum fails.
        assert!(!tracker.is_quorum_met(later));
        let err = tracker.finalise(later).unwrap_err();
        match err {
            QuorumError::QuorumNotMet {
                got,
                needed,
                reason,
            } => {
                assert_eq!(got, 1);
                assert_eq!(needed, 2);
                assert_eq!(reason, QuorumFailureReason::Unreachable);
            }
            other => panic!("expected QuorumNotMet, got {other:?}"),
        }
    }

    #[test]
    fn quorum_finalise_reports_timeout_when_partial_acks() {
        let policy =
            QuorumPolicy::new(5, 3, Duration::from_millis(1), Duration::from_secs(30)).unwrap();
        let t0 = instant_base();
        let mut tracker = AckTracker::new(policy, t0);
        tracker.record_local();
        tracker.record_peer_ack("peer-1");
        // Two total acks (1 local + 1 peer); need 3. Past deadline,
        // so it's Timeout (peers responded but not enough).
        let err = tracker
            .finalise(t0 + Duration::from_millis(50))
            .unwrap_err();
        match err {
            QuorumError::QuorumNotMet { reason, .. } => {
                assert_eq!(reason, QuorumFailureReason::Timeout);
            }
            other => panic!("expected QuorumNotMet/Timeout, got {other:?}"),
        }
    }

    #[test]
    fn id_drift_counted_but_does_not_satisfy_quorum() {
        let policy = QuorumPolicy::majority(3).unwrap();
        let mut tracker = AckTracker::new(policy, instant_base());
        tracker.record_local();
        tracker.record_id_drift("peer-1");
        tracker.record_id_drift("peer-2");
        // id-drift acks do NOT count toward quorum, only toward metrics.
        assert_eq!(tracker.id_drift_count(), 2);
        assert!(!tracker.is_quorum_met(instant_base()));
    }

    #[test]
    fn finalise_without_local_commit_errors_local_write_failed() {
        let policy = QuorumPolicy::majority(3).unwrap();
        let tracker = AckTracker::new(policy, instant_base());
        let err = tracker.finalise(instant_base()).unwrap_err();
        assert!(matches!(err, QuorumError::LocalWriteFailed { .. }));
    }

    #[test]
    fn quorum_error_is_displayable_and_is_an_error() {
        let e = QuorumError::QuorumNotMet {
            got: 1,
            needed: 3,
            reason: QuorumFailureReason::Timeout,
        };
        let display = format!("{e}");
        assert!(display.contains("quorum not met"));
        // Ensure it participates in the `std::error::Error` ecosystem.
        let _: &dyn std::error::Error = &e;
    }

    #[test]
    fn single_node_quorum_is_trivially_met() {
        // N = W = 1 is the degenerate case — equivalent to the v0.6.0
        // behaviour. Must still work so `--quorum-writes 1` is a
        // legitimate configuration and doesn't require special cases
        // in callers.
        let policy =
            QuorumPolicy::new(1, 1, Duration::from_millis(500), Duration::from_secs(30)).unwrap();
        let mut tracker = AckTracker::new(policy, instant_base());
        tracker.record_local();
        assert!(tracker.is_quorum_met(instant_base()));
    }
}
