// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Inbound Ed25519 verification for federated `memory_links` (Track H,
//! Task H3).
//!
//! Builds on H1 ([`crate::identity::keypair`]) and H2
//! ([`crate::identity::sign`]). H2 sealed the canonical CBOR encoding and
//! the outbound signing path; this module is the mirror — when a link
//! arrives from a peer over `sync_push`, we re-derive the same canonical
//! CBOR bytes and verify the 64-byte signature against the public key
//! associated with the link's `observed_by` claim.
//!
//! # Trust model
//!
//! - The peer's public key is read from the *receiver's* on-disk key
//!   directory ([`crate::identity::keypair::default_key_dir`]) — i.e. the
//!   peer was previously enrolled (`identity import` or `identity
//!   generate` for a peer agent_id) by this host's operator. This keeps
//!   the trust root local: a peer cannot upgrade its own attest_level by
//!   sending us a fresh public key.
//! - If `observed_by` has no enrolled key on this host, the link is still
//!   accepted (`attest_level = "unsigned"`) so federation back-compat
//!   holds for peers that haven't enrolled yet. This degraded posture is
//!   intentional — H3 brings opt-in attestation, not a hard cutover.
//! - If the peer *is* enrolled and the signature does not verify, the
//!   link is rejected with a `tracing::warn!` log line. Tampered or
//!   forged inbound links never land in the receiver's `memory_links`
//!   table.
//!
//! # Out of scope here
//!
//! - `attest_level` enum + `memory_verify` MCP tool (H4). H3 stays on
//!   the existing TEXT column with the literal `"peer_attested"` /
//!   `"unsigned"` strings already documented in [`crate::db`].
//! - `signed_events` audit table (H5).
//! - End-to-end federation integration test (H6).

use std::path::Path;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::identity::keypair;
use crate::identity::sign::{SignableLink, canonical_cbor};

/// Length of an Ed25519 signature in bytes. Mirrors the constant
/// [`ed25519_dalek::SIGNATURE_LENGTH`] but pinned locally so the verify
/// path doesn't pull a pub-use dependency on the crate's surface.
pub const SIGNATURE_LEN: usize = ed25519_dalek::SIGNATURE_LENGTH;

/// Outcome of an inbound verify attempt.
///
/// Hand-rolled `Display` + `Error` (no `thiserror`) per repo convention:
/// the OSS substrate keeps its dependency surface deliberately small so
/// the AgenticMem commercial layer can lift the same error shape without
/// re-vendoring proc-macros.
#[derive(Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// Signature did not validate against the supplied public key over
    /// the link's canonical CBOR. Either the link content was tampered
    /// with in flight, the signature bytes themselves were flipped, or
    /// the wrong public key was supplied for `observed_by`.
    Tampered,
    /// `lookup_peer_public_key` returned `None` — the receiver has no
    /// enrolled key for `observed_by`. Callers may choose to treat this
    /// as accept-and-flag-as-unsigned (the federation inbound path) or
    /// as a hard reject (a future strict-mode operator opt-in).
    NoPublicKey,
    /// The supplied signature blob was not exactly 64 bytes — Ed25519
    /// signatures are fixed-length, so any other length is structurally
    /// invalid before we even try the verify.
    MalformedSignature,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tampered => f.write_str(
                "Ed25519 signature did not validate against the supplied public key — \
                 link content or signature bytes do not match what observed_by signed",
            ),
            Self::NoPublicKey => {
                f.write_str("no public key enrolled for observed_by — receiver cannot verify")
            }
            Self::MalformedSignature => f.write_str(
                "signature is not exactly 64 bytes — not a well-formed Ed25519 signature",
            ),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Verify `signature` over the canonical CBOR encoding of `link` using
/// `public`.
///
/// The verifier re-derives the exact byte sequence H2's
/// [`crate::identity::sign::sign`] hashed before signing. Any divergence
/// between the inbound link content and what the peer originally signed
/// — even a single byte flip in `relation`, `observed_by`, etc. —
/// changes the CBOR output and makes Ed25519 reject the signature.
///
/// # Errors
///
/// - [`VerifyError::MalformedSignature`] — `signature.len() != 64`.
/// - [`VerifyError::Tampered`] — signature does not validate against
///   `public` over the canonical CBOR. Same variant covers all
///   "validation failed" cases (wrong key, flipped sig byte, mutated
///   link field) on purpose: the inbound posture is "reject" regardless
///   of *which* of those happened, and surfacing the distinction would
///   leak verification-side timing/structure to a misbehaving peer.
pub fn verify(
    public: &VerifyingKey,
    link: &SignableLink<'_>,
    signature: &[u8],
) -> Result<(), VerifyError> {
    if signature.len() != SIGNATURE_LEN {
        return Err(VerifyError::MalformedSignature);
    }
    let mut sig_arr = [0u8; SIGNATURE_LEN];
    sig_arr.copy_from_slice(signature);
    let sig = Signature::from_bytes(&sig_arr);

    // CBOR encode failures are surfaced as Tampered too — the only way
    // canonical_cbor errors today is a serialization bug, which from the
    // verifier's perspective is functionally equivalent to "we cannot
    // re-derive the bytes the peer signed, so we cannot trust this link".
    let payload = canonical_cbor(link).map_err(|_| VerifyError::Tampered)?;

    public
        .verify(&payload, &sig)
        .map_err(|_| VerifyError::Tampered)
}

/// Look up the public key associated with `observed_by` on this host's
/// on-disk key store.
///
/// Reuses the H1 [`keypair::load`] loader (same path layout: `<key_dir>/
/// <agent_id>.pub`). The loader will succeed for any `agent_id` whose
/// public-key file is present — it does not require the `.priv` file
/// (this host has no reason to hold a peer's private key, only the
/// matching public key it received via `identity import`).
///
/// Returns `None` when:
/// - `observed_by` is the empty string,
/// - the key directory cannot be resolved (extremely rare; only when the
///   OS does not advertise a config dir),
/// - no `<observed_by>.pub` file exists under the key directory,
/// - the on-disk file is malformed (length mismatch, etc.).
///
/// In every `None` case the caller should fall back to the
/// accept-and-flag-as-unsigned posture rather than rejecting the link.
#[must_use]
pub fn lookup_peer_public_key(observed_by: &str) -> Option<VerifyingKey> {
    if observed_by.is_empty() {
        return None;
    }
    let dir = keypair::default_key_dir().ok()?;
    lookup_peer_public_key_in(observed_by, &dir)
}

/// Variant of [`lookup_peer_public_key`] that takes an explicit key
/// directory. Used by tests so we can populate a tempdir with peer
/// public keys without touching the operator's real `~/.config/ai-memory`.
/// Callers in production code should prefer [`lookup_peer_public_key`]
/// so the storage location stays uniform across `keypair`, `sign`, and
/// `verify`.
#[must_use]
pub fn lookup_peer_public_key_in(observed_by: &str, dir: &Path) -> Option<VerifyingKey> {
    if observed_by.is_empty() {
        return None;
    }
    keypair::load(observed_by, dir).ok().map(|kp| kp.public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::keypair as kp_mod;
    use crate::identity::sign;
    use tempfile::TempDir;

    fn link_fixture() -> SignableLink<'static> {
        SignableLink {
            src_id: "src-001",
            dst_id: "dst-002",
            relation: "related_to",
            observed_by: Some("alice"),
            valid_from: Some("2026-05-05T00:00:00+00:00"),
            valid_until: None,
        }
    }

    #[test]
    fn verify_accepts_valid_signature() {
        // Happy path: alice signs, verifier holds alice.pub → accept.
        let alice = kp_mod::generate("alice").unwrap();
        let link = link_fixture();
        let sig = sign::sign(&alice, &link).unwrap();
        verify(&alice.public, &link, &sig).expect("happy-path verify must succeed");
    }

    #[test]
    fn verify_rejects_flipped_signature_byte() {
        // Single bit flip in the signature → Tampered. Ed25519 has no
        // malleability window — any altered byte invalidates.
        let alice = kp_mod::generate("alice").unwrap();
        let link = link_fixture();
        let mut sig = sign::sign(&alice, &link).unwrap();
        sig[0] ^= 0x01;
        let err = verify(&alice.public, &link, &sig).unwrap_err();
        assert_eq!(err, VerifyError::Tampered, "flipped sig byte must reject");
    }

    #[test]
    fn verify_rejects_mutated_link_content() {
        // Re-sign with relation=related_to, but verifier sees relation=
        // supersedes (same other fields). CBOR re-encoding produces a
        // different byte stream → Ed25519 rejects.
        let alice = kp_mod::generate("alice").unwrap();
        let original = link_fixture();
        let sig = sign::sign(&alice, &original).unwrap();

        let mut tampered = original.clone();
        tampered.relation = "supersedes";
        let err = verify(&alice.public, &tampered, &sig).unwrap_err();
        assert_eq!(
            err,
            VerifyError::Tampered,
            "mutated link content must reject"
        );
    }

    #[test]
    fn verify_rejects_wrong_pubkey() {
        // Signed by alice, attempted-verified with bob's pubkey →
        // Tampered. The variant deliberately doesn't distinguish "wrong
        // key" from "tampered content" so a misbehaving peer can't
        // probe which fields the verifier touched.
        let alice = kp_mod::generate("alice").unwrap();
        let bob = kp_mod::generate("bob").unwrap();
        let link = link_fixture();
        let sig = sign::sign(&alice, &link).unwrap();
        let err = verify(&bob.public, &link, &sig).unwrap_err();
        assert_eq!(err, VerifyError::Tampered);
    }

    #[test]
    fn verify_rejects_short_signature() {
        let alice = kp_mod::generate("alice").unwrap();
        let link = link_fixture();
        // 32 bytes is wrong (Ed25519 wants 64).
        let short = vec![0u8; 32];
        let err = verify(&alice.public, &link, &short).unwrap_err();
        assert_eq!(err, VerifyError::MalformedSignature);
    }

    #[test]
    fn verify_rejects_long_signature() {
        let alice = kp_mod::generate("alice").unwrap();
        let link = link_fixture();
        // 128 bytes is wrong (Ed25519 wants 64).
        let long = vec![0u8; 128];
        let err = verify(&alice.public, &link, &long).unwrap_err();
        assert_eq!(err, VerifyError::MalformedSignature);
    }

    #[test]
    fn verify_rejects_empty_signature() {
        let alice = kp_mod::generate("alice").unwrap();
        let link = link_fixture();
        let err = verify(&alice.public, &link, &[]).unwrap_err();
        assert_eq!(err, VerifyError::MalformedSignature);
    }

    #[test]
    fn lookup_peer_public_key_in_returns_none_for_unknown() {
        let dir = TempDir::new().unwrap();
        // Empty key dir → no enrolled peer.
        assert!(lookup_peer_public_key_in("alice", dir.path()).is_none());
    }

    #[test]
    fn lookup_peer_public_key_in_returns_none_for_empty_id() {
        let dir = TempDir::new().unwrap();
        assert!(lookup_peer_public_key_in("", dir.path()).is_none());
    }

    #[test]
    fn lookup_peer_public_key_in_finds_enrolled_pubkey() {
        // Mirror an `identity import` for a peer: write only the .pub
        // file under the key dir. lookup must return the same key.
        let dir = TempDir::new().unwrap();
        let alice = kp_mod::generate("alice").unwrap();
        let pub_only = kp_mod::AgentKeypair {
            agent_id: "alice".to_string(),
            public: alice.public,
            private: None,
        };
        kp_mod::save_public_only(&pub_only, dir.path()).unwrap();
        let found = lookup_peer_public_key_in("alice", dir.path()).expect("lookup hit");
        assert_eq!(found.to_bytes(), alice.public.to_bytes());
    }

    #[test]
    fn lookup_peer_public_key_in_finds_full_keypair_pub() {
        // A self-generated agent (with both .pub and .priv on disk) is
        // also a valid lookup target — useful in single-host loopback
        // tests where the same agent both signs and verifies.
        let dir = TempDir::new().unwrap();
        let alice = kp_mod::generate("alice").unwrap();
        kp_mod::save(&alice, dir.path()).unwrap();
        let found = lookup_peer_public_key_in("alice", dir.path()).expect("lookup hit");
        assert_eq!(found.to_bytes(), alice.public.to_bytes());
    }

    #[test]
    fn lookup_peer_public_key_in_skips_invalid_agent_id() {
        // `keypair::load` validates the agent_id; lookup should not
        // panic and should report `None` for invalid input.
        let dir = TempDir::new().unwrap();
        assert!(lookup_peer_public_key_in("has space", dir.path()).is_none());
        assert!(lookup_peer_public_key_in("has\0null", dir.path()).is_none());
    }

    #[test]
    fn end_to_end_peer_a_signs_peer_b_verifies() {
        // Two-host simulation: alice signs on host A; host B has only
        // alice.pub enrolled (no .priv). Host B looks up alice's pubkey
        // and verifies — passes.
        let host_b_keys = TempDir::new().unwrap();
        let alice = kp_mod::generate("alice").unwrap();

        // Host B operator imports alice's public key.
        let alice_pub_for_b = kp_mod::AgentKeypair {
            agent_id: "alice".to_string(),
            public: alice.public,
            private: None,
        };
        kp_mod::save_public_only(&alice_pub_for_b, host_b_keys.path()).unwrap();

        // Alice signs a link on host A.
        let link = link_fixture();
        let sig = sign::sign(&alice, &link).unwrap();

        // Host B receives the link, looks up alice's pubkey, verifies.
        let key_on_b =
            lookup_peer_public_key_in("alice", host_b_keys.path()).expect("alice enrolled on B");
        verify(&key_on_b, &link, &sig).expect("cross-host verify must succeed");
    }

    #[test]
    fn end_to_end_no_pubkey_returns_none_for_caller_to_handle() {
        // Host B has no key enrolled for alice → lookup returns None.
        // The caller (federation inbound) is responsible for the
        // accept-and-flag-as-unsigned posture; verify() is not invoked.
        let host_b_keys = TempDir::new().unwrap();
        assert!(lookup_peer_public_key_in("alice", host_b_keys.path()).is_none());
    }

    #[test]
    fn verify_error_display_messages_are_distinct() {
        // Sanity: each variant has a non-empty, distinct human message.
        let m_t = format!("{}", VerifyError::Tampered);
        let m_n = format!("{}", VerifyError::NoPublicKey);
        let m_m = format!("{}", VerifyError::MalformedSignature);
        assert!(!m_t.is_empty());
        assert!(!m_n.is_empty());
        assert!(!m_m.is_empty());
        assert_ne!(m_t, m_n);
        assert_ne!(m_n, m_m);
        assert_ne!(m_t, m_m);
    }
}
