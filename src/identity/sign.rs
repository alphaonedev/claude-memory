// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Outbound Ed25519 signing for `memory_links` (Track H, Task H2).
//!
//! Builds on H1 ([`crate::identity::keypair`]) — the per-agent
//! [`AgentKeypair`] is the signing key. This module provides the two
//! pieces H2 ships:
//!
//! 1. [`canonical_cbor`] — RFC 8949 §4.2.1 deterministic CBOR encoding
//!    of the six link fields the signature commits to:
//!    `src_id`, `dst_id`, `relation`, `observed_by`, `valid_from`,
//!    `valid_until`. Same bytes on every host, every architecture,
//!    every endianness — the precondition for round-tripping a
//!    signature through the federation wire.
//! 2. [`sign`] — wraps `canonical_cbor` + Ed25519 over the resulting
//!    bytes. Returns the 64-byte signature ready to drop into the
//!    `signature` BLOB column on `memory_links`.
//!
//! H3 will mirror [`canonical_cbor`] on the inbound path so verification
//! re-derives the same bytes from the inbound row before checking the
//! signature against the peer's public key.
//!
//! # Why CBOR?
//!
//! CBOR is the RustCrypto / IETF default for signed payloads (COSE
//! lives on top of CBOR). RFC 8949 §4.2.1 defines a *deterministic*
//! encoding: map keys sort lexicographically, integers use the smallest
//! length, no indefinite-length items, no semantic tags we don't need.
//! That gives us byte-stable input to Ed25519 without writing a custom
//! binary format and without depending on `serde_json`'s key-ordering
//! quirks (which are not part of its public contract).
//!
//! # Out of scope here
//!
//! - Inbound verification (H3).
//! - `attest_level` enum + `memory_verify` MCP tool (H4).
//! - `signed_events` audit table (H5).

use anyhow::{Context, Result};
use ed25519_dalek::Signer;

use crate::identity::keypair::AgentKeypair;

/// The six fields the link signature commits to.
///
/// Decoupled from [`crate::models::MemoryLink`] on purpose: that struct
/// is the public wire shape for `get_links` (4 columns), while the
/// signed bundle includes the temporal-validity columns (`valid_from`,
/// `valid_until`, `observed_by`) added in v0.6.3 schema v15. Keeping
/// `SignableLink` separate means H3's verifier can deserialize directly
/// from a row without dragging the entire `MemoryLink` shape — and it
/// gives the canonical encoder a single, audited shape to commit to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignableLink<'a> {
    pub src_id: &'a str,
    pub dst_id: &'a str,
    pub relation: &'a str,
    /// Agent that observed / asserted this link. `None` when the link
    /// was created by an unidentified caller (rare on the signing path
    /// — the keypair's owner is normally the observer).
    pub observed_by: Option<&'a str>,
    /// RFC3339 instant the link became true. Always present on writes
    /// produced by `db::create_link` (set to "now" at insert time).
    pub valid_from: Option<&'a str>,
    /// RFC3339 instant the link was invalidated, or `None` if still
    /// valid. Almost always `None` at insert time; set later by
    /// `db::invalidate_link`.
    pub valid_until: Option<&'a str>,
}

/// RFC 8949 §4.2.1 deterministic CBOR encoding of the six signable
/// link fields.
///
/// The encoded shape is a CBOR map with 6 entries keyed by the field
/// names below. Map keys are emitted in sort order (per RFC 8949 §4.2.1
/// "Core Deterministic Encoding"), integers use the shortest form, and
/// `Option::None` is encoded as CBOR `null`. Encoding the same
/// `SignableLink` twice (or on a different host) produces identical
/// bytes — the precondition Ed25519 needs.
///
/// Field order matters at the *byte* level even though `ciborium`
/// canonicalises map keys for us — we still pass a deterministic
/// `Vec<(&str, ...)>` shape to keep this function's intent reviewable
/// without leaning on a non-obvious property of the encoder.
///
/// # Errors
///
/// Returns an error only when CBOR serialization fails — in practice
/// unreachable for the fixed-shape input above, but surfaced as a
/// `Result` so callers don't have to choose between panicking and
/// silently signing a truncated payload.
pub fn canonical_cbor(link: &SignableLink<'_>) -> Result<Vec<u8>> {
    // We model the payload as a plain `BTreeMap<&str, ciborium::Value>`
    // so map-key ordering is enforced at construction time — the encoder
    // walks the BTreeMap in iteration order, which matches lexicographic
    // sort. ciborium's `into_writer` emits canonical (smallest-int,
    // definite-length) representations by default.
    use std::collections::BTreeMap;
    let mut map: BTreeMap<&str, ciborium::Value> = BTreeMap::new();
    map.insert("src_id", ciborium::Value::Text(link.src_id.to_string()));
    map.insert("dst_id", ciborium::Value::Text(link.dst_id.to_string()));
    map.insert("relation", ciborium::Value::Text(link.relation.to_string()));
    map.insert("observed_by", text_or_null(link.observed_by));
    map.insert("valid_from", text_or_null(link.valid_from));
    map.insert("valid_until", text_or_null(link.valid_until));

    // Convert the BTreeMap to a `ciborium::Value::Map` whose entries are
    // already in lexicographic key order. ciborium will preserve that
    // order on the wire — the documented default for `into_writer`.
    let entries: Vec<(ciborium::Value, ciborium::Value)> = map
        .into_iter()
        .map(|(k, v)| (ciborium::Value::Text(k.to_string()), v))
        .collect();
    let value = ciborium::Value::Map(entries);

    let mut out: Vec<u8> = Vec::with_capacity(128);
    ciborium::ser::into_writer(&value, &mut out).context("CBOR encode SignableLink")?;
    Ok(out)
}

/// Sign `link` with `keypair`'s private key.
///
/// Encodes the link via [`canonical_cbor`], then runs Ed25519 over the
/// resulting bytes. Returns the 64-byte signature, ready to drop into
/// the `signature` BLOB column on `memory_links`.
///
/// # Errors
///
/// - `keypair.private` is `None` (public-only handle — verification
///   only).
/// - The CBOR encoding step fails (in practice unreachable; surfaced
///   for completeness).
pub fn sign(keypair: &AgentKeypair, link: &SignableLink<'_>) -> Result<Vec<u8>> {
    let signing = keypair.private.as_ref().with_context(|| {
        format!(
            "AgentKeypair for {} has no private key — cannot sign",
            keypair.agent_id
        )
    })?;
    let bytes = canonical_cbor(link)?;
    let sig = signing.sign(&bytes);
    Ok(sig.to_bytes().to_vec())
}

/// Helper: lift `Option<&str>` into a CBOR `Text` or `Null`. Encoding
/// `None` as `null` (rather than dropping the key) keeps the map's key
/// set fixed across rows — H3's verifier can re-derive the bytes
/// without branching on which optional fields were present.
fn text_or_null(opt: Option<&str>) -> ciborium::Value {
    match opt {
        Some(s) => ciborium::Value::Text(s.to_string()),
        None => ciborium::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::keypair;
    use ed25519_dalek::Verifier;

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
    fn canonical_cbor_is_deterministic() {
        // RFC 8949 §4.2.1 — encoding the same logical input twice must
        // produce identical bytes. This is the round-trip precondition
        // for Ed25519 signing.
        let link = link_fixture();
        let a = canonical_cbor(&link).expect("encode");
        let b = canonical_cbor(&link).expect("encode");
        assert_eq!(a, b, "deterministic CBOR must be byte-stable");
    }

    #[test]
    fn canonical_cbor_differs_on_field_change() {
        // Sanity-check that the encoder isn't flattening fields. Any
        // change in the signed surface should change the byte output.
        let base = link_fixture();
        let mut altered = base.clone();
        altered.relation = "supersedes";
        let a = canonical_cbor(&base).expect("encode base");
        let b = canonical_cbor(&altered).expect("encode altered");
        assert_ne!(a, b, "different relation must produce different bytes");
    }

    #[test]
    fn canonical_cbor_handles_all_optionals_none() {
        let link = SignableLink {
            src_id: "s",
            dst_id: "d",
            relation: "r",
            observed_by: None,
            valid_from: None,
            valid_until: None,
        };
        let bytes = canonical_cbor(&link).expect("encode");
        assert!(!bytes.is_empty());
        // Two encodes still match.
        assert_eq!(bytes, canonical_cbor(&link).expect("re-encode"));
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let kp = keypair::generate("alice").expect("generate");
        let link = link_fixture();
        let sig_bytes = sign(&kp, &link).expect("sign");
        assert_eq!(sig_bytes.len(), 64, "Ed25519 signatures are 64 bytes");

        // Re-derive the canonical bytes and verify with the public key.
        let payload = canonical_cbor(&link).expect("encode");
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
        kp.public.verify(&payload, &sig).expect("verify");
    }

    #[test]
    fn sign_refuses_public_only_keypair() {
        // Public-only handles (load() with no .priv on disk, or list())
        // must not be silently treated as zero-byte signatures — the
        // caller has to fall back to the unsigned path explicitly.
        let kp = keypair::generate("alice").unwrap();
        let pub_only = AgentKeypair {
            agent_id: "alice".to_string(),
            public: kp.public,
            private: None,
        };
        let err = sign(&pub_only, &link_fixture()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no private key"), "got: {msg}");
    }

    #[test]
    fn sign_differs_for_different_keys() {
        // Two keypairs over the same link produce different signatures
        // (nondeterministic randomness, plus distinct keys).
        let alice = keypair::generate("alice").unwrap();
        let bob = keypair::generate("bob").unwrap();
        let link = link_fixture();
        let sig_a = sign(&alice, &link).unwrap();
        let sig_b = sign(&bob, &link).unwrap();
        assert_ne!(sig_a, sig_b);
    }

    #[test]
    fn signature_does_not_verify_against_other_pub() {
        let alice = keypair::generate("alice").unwrap();
        let bob = keypair::generate("bob").unwrap();
        let link = link_fixture();
        let sig_bytes = sign(&alice, &link).unwrap();
        let payload = canonical_cbor(&link).unwrap();
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
        // Alice's signature must not verify under Bob's public key.
        assert!(bob.public.verify(&payload, &sig).is_err());
    }
}
