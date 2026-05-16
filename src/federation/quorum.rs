// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Quorum finalisation and error payload serialisation.

use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::replication::{AckTracker, QuorumError, QuorumFailureReason};

/// Classify an `AckTracker` into either a committed quorum (`Ok(n)`) or
/// an error with a reason suitable for the `/503 quorum_not_met`
/// payload. Consumes the tracker — call after the broadcast loop.
///
/// # Errors
///
/// Returns `QuorumError::QuorumNotMet` if the tracker did not meet
/// its W threshold by the `now` tick.
pub fn finalise_quorum(tracker: &AckTracker) -> Result<usize, QuorumError> {
    tracker.finalise(Instant::now())
}

/// Serialised 503 payload for failed quorum writes.
#[derive(Debug, Serialize, Deserialize)]
pub struct QuorumNotMetPayload {
    pub error: &'static str,
    pub got: usize,
    pub needed: usize,
    pub reason: String,
}

impl QuorumNotMetPayload {
    #[must_use]
    pub fn from_err(err: &QuorumError) -> Self {
        match err {
            QuorumError::QuorumNotMet {
                got,
                needed,
                reason,
            } => Self {
                error: "quorum_not_met",
                got: *got,
                needed: *needed,
                // InFlight shouldn't surface in the HTTP payload — the
                // broadcast loop waits until the deadline before
                // calling finalise(). If a caller somehow gets it here,
                // we map to "timeout" for the operator-facing 503 so
                // we don't leak a transient internal state as a fourth
                // public string.
                reason: match reason {
                    QuorumFailureReason::Unreachable => "unreachable".to_string(),
                    QuorumFailureReason::Timeout | QuorumFailureReason::InFlight => {
                        "timeout".to_string()
                    }
                    QuorumFailureReason::IdDrift => "id_drift".to_string(),
                },
            },
            QuorumError::InvalidPolicy { detail } => Self {
                error: "quorum_not_met",
                got: 0,
                needed: 0,
                reason: format!("invalid_policy:{detail}"),
            },
            QuorumError::LocalWriteFailed { detail } => Self {
                error: "quorum_not_met",
                got: 0,
                needed: 0,
                reason: format!("local_write_failed:{detail}"),
            },
        }
    }
}
