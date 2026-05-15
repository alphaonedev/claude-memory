// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 federation security — peer attestation substrate for
//! `/api/v1/sync/push` (issue #238). The companion `/sync/since`
//! scope-allowlist machinery for issue #239 lands in the next
//! commit on this branch.
//!
//! ## Gap context (red-team #230, issue #238)
//!
//! `SyncPushBody::sender_agent_id` is a body-claimed identity.
//! Pre-v0.7.0 the receiver logged it for audit and used it to charge
//! per-agent quotas, but never attested it against anything. A peer
//! with a valid mTLS cert could claim ANY `agent_id` in the body,
//! defeating per-agent audit-trail integrity.
//!
//! ## Substrate honesty (operator-must-read)
//!
//! The cryptographic anchor for "this connection is from an authorised
//! peer" today is the mTLS client-cert fingerprint pin
//! (`src/tls.rs::FingerprintAllowlistVerifier`). axum-server 0.8 does
//! **not** propagate the verified peer certificate (or its SAN/CN) to
//! axum handlers — there is no per-request extension that exposes the
//! rustls server connection. Closing that gap requires either a
//! non-trivial axum-server PR or a new x509-parser dependency wired
//! into a custom `ClientCertVerifier` that stashes per-connection
//! state. **That work is escalated to v0.8.0** and tracked under the
//! follow-up to issue #238 in the PR body that landed this module.
//!
//! What this module DOES give v0.7.0:
//!
//! 1. A NEW required outbound header `x-peer-id` carrying the peer's
//!    self-claim of its `sender_agent_id`. The federation client
//!    (`src/federation/sync.rs::post_once`) attaches it on every
//!    outbound `/sync/push` request. The receiver cross-checks
//!    `body.sender_agent_id` against this header — the body field can
//!    no longer silently disagree with the wire-level peer-id without
//!    an explicit operator override.
//! 2. An operator-configured allowlist that binds **claimed peer-id**
//!    to **allowed sender_agent_ids**. Loaded from the env var
//!    `AI_MEMORY_FED_PEER_ATTESTATION` (JSON; see
//!    [`PeerAttestationConfig::from_env`] for the schema). Peers not
//!    in the allowlist still get a clear refusal envelope.
//! 3. An opt-in env bypass (`AI_MEMORY_FED_TRUST_BODY_AGENT_ID=1`) so
//!    the live Mac Mini test cell and the DigitalOcean campaign keep
//!    working without config updates.
//!
//! The end-to-end trust chain in v0.7.0 is therefore:
//!
//! ```text
//! Operator configures mTLS allowlist (fingerprints)
//!  └─ rustls verifies peer client cert at handshake
//!     └─ HTTP request reaches handler ONLY if cert was pinned
//!        └─ handler reads `x-peer-id` header (operator-bound to
//!           fingerprints via deployment runbook, NOT cryptographic-
//!           ally tied to the cert TODAY)
//!           └─ this module validates body.sender_agent_id.
//! ```
//!
//! The weak link is the operator-bound binding between fingerprint
//! and `x-peer-id`. v0.8.0 will replace that with the cert-SAN
//! attestation surface and remove this caveat.
//!
//! ## Note on the `allowed_namespaces` field
//!
//! `PeerScope` carries an `allowed_namespaces: Vec<String>` field that
//! this commit does not yet read. The field is added now (rather than
//! in the #239 commit) so the operator-facing
//! `AI_MEMORY_FED_PEER_ATTESTATION` JSON schema is stable across the
//! two security commits — operators don't need to migrate their
//! allowlist file between v0.7.0 substrate updates. The #239 commit
//! consumes the field via the `/sync/since` scope filter it ships.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Env var carrying the operator's per-peer attestation allowlist
/// (JSON). Absent / parse-error = empty allowlist. The #239 commit
/// extends the read-side use of this map to gate `/sync/since`
/// projections by namespace; this commit reads only the
/// `allowed_sender_agent_ids` member of each entry.
pub const PEER_ATTESTATION_ENV: &str = "AI_MEMORY_FED_PEER_ATTESTATION";

/// Env var that, when set to `"1"`, disables the #238 attestation
/// check and reverts `/sync/push` to its pre-v0.7.0 posture (accept
/// any body-claimed `sender_agent_id`). Backwards-compat for test
/// cells where the operator hasn't yet wired the allowlist.
pub const TRUST_BODY_AGENT_ID_ENV: &str = "AI_MEMORY_FED_TRUST_BODY_AGENT_ID";

/// HTTP header carrying the peer's self-claim of `sender_agent_id`.
/// Lowercase per the HTTP/2 wire convention; axum's `HeaderMap`
/// lookups are case-insensitive.
pub const PEER_ID_HEADER: &str = "x-peer-id";

/// Allowlist row for a single peer (keyed by claimed peer-id).
///
/// The `allowed_sender_agent_ids` field, when empty, is interpreted
/// as "peer may push memories where `body.sender_agent_id` equals
/// the peer-id itself" — the minimal-trust default for a peer that
/// only authors as itself. When non-empty, it overrides that default
/// and the list (exact strings, no glob) is the authoritative set of
/// `body.sender_agent_id` values the peer may claim.
///
/// `allowed_namespaces` is reserved for the issue #239 follow-up
/// commit on this branch — schema is fixed now so operator-facing
/// JSON does not churn between security commits. See the module
/// docs for the staging rationale.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PeerScope {
    /// Exact `body.sender_agent_id` values this peer may claim on
    /// `/sync/push`. Empty = only the peer-id itself.
    #[serde(default)]
    pub allowed_sender_agent_ids: Vec<String>,
    /// Reserved for issue #239 — `/sync/since` namespace scope.
    /// Operators may populate it now; the read-side gate lands in
    /// the next commit on this branch.
    #[serde(default)]
    pub allowed_namespaces: Vec<String>,
}

/// Operator-configured federation peer-attestation map. Loaded from
/// the [`PEER_ATTESTATION_ENV`] env var as JSON:
///
/// ```json
/// {
///   "peer-node-1": {
///     "allowed_sender_agent_ids": ["ai:peer-node-1@host", "alice"],
///     "allowed_namespaces": ["public/*", "shared/team-x/**"]
///   },
///   "peer-node-2": {
///     "allowed_namespaces": ["public/*"]
///   }
/// }
/// ```
///
/// The empty map (`{}` or no env var at all) is a valid state. It
/// triggers the "header must equal body" posture on `/sync/push`.
#[derive(Clone, Debug, Default)]
pub struct PeerAttestationConfig {
    pub peers: HashMap<String, PeerScope>,
}

/// Reason a body-claimed `sender_agent_id` failed attestation against
/// the wire-level `x-peer-id` header.
#[derive(Debug, Clone)]
pub enum AttestError {
    /// `x-peer-id` header absent AND env bypass NOT set. Caller
    /// should return 403.
    HeaderMissing,
    /// `x-peer-id` header present, body field present, no allowlist
    /// row exists for this peer-id, AND `body.sender_agent_id` does
    /// not equal the header. The peer is claiming an identity it has
    /// no operator-configured permission to claim.
    Mismatch {
        claimed: String,
        peer_header: String,
    },
}

impl AttestError {
    /// Stable machine-readable tag for the error envelope.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::HeaderMissing => "peer_id_header_missing",
            Self::Mismatch { .. } => "sender_agent_id_mismatch",
        }
    }
}

impl PeerAttestationConfig {
    /// Load the allowlist from the [`PEER_ATTESTATION_ENV`] env var.
    /// Missing env var = empty config (default-deny on cross-author
    /// claims). Parse error = empty config + a `tracing::warn!` so
    /// the operator sees the typo immediately. Refusing to start on
    /// a malformed allowlist would be a self-DOS hazard during config
    /// rollouts.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(PEER_ATTESTATION_ENV) {
            Ok(s) if !s.trim().is_empty() => {
                match serde_json::from_str::<HashMap<String, PeerScope>>(&s) {
                    Ok(peers) => Self { peers },
                    Err(e) => {
                        tracing::warn!(
                            target: "federation::peer_attestation",
                            env = PEER_ATTESTATION_ENV,
                            error = %e,
                            "failed to parse peer-attestation env var as JSON — \
                             falling back to empty allowlist"
                        );
                        Self::default()
                    }
                }
            }
            _ => Self::default(),
        }
    }

    /// Lookup scope for a claimed peer-id. Returns `None` when the
    /// operator has not configured any row for this peer.
    #[must_use]
    pub fn scope_for(&self, peer_id: &str) -> Option<&PeerScope> {
        self.peers.get(peer_id)
    }
}

/// Whether the operator has explicitly opted out of #238 attestation
/// (legacy behaviour: trust the body field).
#[must_use]
pub fn trust_body_agent_id_bypass() -> bool {
    matches!(std::env::var(TRUST_BODY_AGENT_ID_ENV).as_deref(), Ok("1"))
}

/// #238 attestation core.
///
/// Validates that the body-claimed `sender_agent_id` is one this
/// peer (identified by the `x-peer-id` header) is operator-permitted
/// to claim.
///
/// Decision matrix:
///
/// | `peer_header` | `body_sender`         | allowlist row | result            |
/// |---------------|-----------------------|---------------|-------------------|
/// | `None`        | any                   | n/a           | [`AttestError::HeaderMissing`] |
/// | `Some(p)`     | `None` or empty       | n/a           | Ok (legacy unauthored push) |
/// | `Some(p)`     | `Some(s)` where `s == p` | n/a        | Ok (peer authoring as itself) |
/// | `Some(p)`     | `Some(s)` where `s != p` | None        | [`AttestError::Mismatch`] |
/// | `Some(p)`     | `Some(s)` where `s != p` | Some(scope), `s ∈ scope.allowed_sender_agent_ids` | Ok |
/// | `Some(p)`     | `Some(s)` where `s != p` | Some(scope), `s ∉ scope.allowed_sender_agent_ids` | [`AttestError::Mismatch`] |
///
/// `body_sender == Some("")` is treated as `None` to match the wire
/// reality (federation clients pre-v0.7.0 sometimes serialise the
/// field as the empty string instead of omitting it).
///
/// # Errors
///
/// Returns [`AttestError`] when the attestation contract is violated;
/// callers should render 403 with a structured error envelope.
pub fn attest_sender(
    peer_header: Option<&str>,
    body_sender: Option<&str>,
    config: &PeerAttestationConfig,
) -> Result<(), AttestError> {
    let peer = match peer_header.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => p,
        None => return Err(AttestError::HeaderMissing),
    };
    let claimed = match body_sender.map(str::trim).filter(|s| !s.is_empty()) {
        Some(c) => c,
        // Legacy push with no body claim — peer is implicitly authoring as itself.
        None => return Ok(()),
    };
    if claimed == peer {
        return Ok(());
    }
    if let Some(scope) = config.scope_for(peer)
        && scope
            .allowed_sender_agent_ids
            .iter()
            .any(|a| a.as_str() == claimed)
    {
        return Ok(());
    }
    Err(AttestError::Mismatch {
        claimed: claimed.to_string(),
        peer_header: peer.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(rows: &[(&str, PeerScope)]) -> PeerAttestationConfig {
        let peers = rows
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect();
        PeerAttestationConfig { peers }
    }

    // ---- attest_sender ---------------------------------------------------

    #[test]
    fn attest_header_missing_errors() {
        let cfg = PeerAttestationConfig::default();
        let err = attest_sender(None, Some("alice"), &cfg).unwrap_err();
        assert!(matches!(err, AttestError::HeaderMissing));
        assert_eq!(err.tag(), "peer_id_header_missing");
    }

    #[test]
    fn attest_header_empty_treated_as_missing() {
        let cfg = PeerAttestationConfig::default();
        let err = attest_sender(Some("   "), Some("alice"), &cfg).unwrap_err();
        assert!(matches!(err, AttestError::HeaderMissing));
    }

    #[test]
    fn attest_body_missing_passes_legacy_unauthored() {
        // No body-claimed sender + peer header present = legacy pre-v0.7.0
        // peer that didn't author rows. Accept.
        let cfg = PeerAttestationConfig::default();
        attest_sender(Some("peer-1"), None, &cfg).unwrap();
        attest_sender(Some("peer-1"), Some(""), &cfg).unwrap();
    }

    #[test]
    fn attest_self_authoring_passes() {
        let cfg = PeerAttestationConfig::default();
        attest_sender(Some("peer-1"), Some("peer-1"), &cfg).unwrap();
    }

    #[test]
    fn attest_mismatch_no_allowlist_errors() {
        let cfg = PeerAttestationConfig::default();
        let err = attest_sender(Some("peer-1"), Some("alice"), &cfg).unwrap_err();
        match err {
            AttestError::Mismatch {
                claimed,
                peer_header,
            } => {
                assert_eq!(claimed, "alice");
                assert_eq!(peer_header, "peer-1");
            }
            other => panic!("expected Mismatch, got: {other:?}"),
        }
    }

    #[test]
    fn attest_mismatch_with_matching_allowlist_passes() {
        let cfg = cfg(&[(
            "peer-1",
            PeerScope {
                allowed_sender_agent_ids: vec!["alice".to_string(), "bob".to_string()],
                ..PeerScope::default()
            },
        )]);
        attest_sender(Some("peer-1"), Some("alice"), &cfg).unwrap();
        attest_sender(Some("peer-1"), Some("bob"), &cfg).unwrap();
    }

    #[test]
    fn attest_mismatch_outside_allowlist_errors() {
        let cfg = cfg(&[(
            "peer-1",
            PeerScope {
                allowed_sender_agent_ids: vec!["alice".to_string()],
                ..PeerScope::default()
            },
        )]);
        let err = attest_sender(Some("peer-1"), Some("eve"), &cfg).unwrap_err();
        assert!(matches!(err, AttestError::Mismatch { .. }));
    }

    // ---- PeerAttestationConfig::from_env --------------------------------

    #[test]
    fn from_env_absent_is_empty() {
        unsafe { std::env::remove_var(PEER_ATTESTATION_ENV) };
        let cfg = PeerAttestationConfig::from_env();
        assert!(cfg.peers.is_empty());
    }

    #[test]
    fn from_env_parses_valid_json() {
        let body = r#"{
            "peer-1": {
                "allowed_sender_agent_ids": ["alice", "bob"]
            }
        }"#;
        unsafe { std::env::set_var(PEER_ATTESTATION_ENV, body) };
        let cfg = PeerAttestationConfig::from_env();
        unsafe { std::env::remove_var(PEER_ATTESTATION_ENV) };
        let scope = cfg.scope_for("peer-1").expect("peer-1 row present");
        assert_eq!(scope.allowed_sender_agent_ids, vec!["alice", "bob"]);
    }

    #[test]
    fn from_env_parse_error_is_empty() {
        unsafe { std::env::set_var(PEER_ATTESTATION_ENV, "not json{{") };
        let cfg = PeerAttestationConfig::from_env();
        unsafe { std::env::remove_var(PEER_ATTESTATION_ENV) };
        assert!(cfg.peers.is_empty());
    }
}
