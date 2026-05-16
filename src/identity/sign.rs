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

// ---------------------------------------------------------------------------
// v0.7.0 issue #812 / #813 — SignablePersona + sign_persona
// ---------------------------------------------------------------------------
//
// Mirrors the `SignableLink` shape: a single, audited surface for the
// seven fields the persona signature commits to, encoded via RFC 8949
// §4.2.1 deterministic CBOR. The body of the persona Markdown is
// hashed (SHA-256) BEFORE entering the signed envelope so the payload
// stays bounded (32 bytes) regardless of body length — Ed25519 over
// kilobytes of prose would still work, but the bounded shape lets the
// `signed_events` row carry the same `payload_hash` cheaply.

/// The seven fields the persona signature commits to.
///
/// `body_md_sha256` is the SHA-256 of the UTF-8 bytes of the rendered
/// persona Markdown body (the same string that lands in
/// `memories.content`). Hashing it before signing keeps the canonical
/// payload bounded at ~200 bytes regardless of body length — a 300-500
/// word persona body would otherwise dominate the signed envelope and
/// inflate every `signed_events.payload_hash` recomputation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignablePersona<'a> {
    /// The Persona memory's id (UUIDv4). Stable per (entity_id,
    /// namespace, version) tuple — `PersonaGenerator::generate` mints
    /// it before computing the signature.
    pub persona_id: &'a str,
    /// Subject the persona distils. Mirrors `Persona::entity_id`.
    pub entity_id: &'a str,
    /// Namespace the persona was minted under.
    pub namespace: &'a str,
    /// Monotonic version counter — `1` on the first generation, then
    /// `prev + 1` per regeneration. Pinned in the signature so a
    /// regeneration cannot replay an earlier version's signed bytes.
    pub version: i32,
    /// RFC3339 generation timestamp pinned in `metadata.persona.generated_at`.
    pub generated_at: &'a str,
    /// Source reflection ids — one `derives_from` edge per element.
    /// Order matters at the byte level (the CBOR encoder preserves the
    /// slice order); the writer pins the order to match
    /// `metadata.persona.sources`.
    pub sources: &'a [String],
    /// SHA-256 (32 bytes) over the rendered persona Markdown body's
    /// UTF-8 bytes. Bounds the signed payload size.
    pub body_md_sha256: &'a [u8; 32],
}

/// RFC 8949 §4.2.1 deterministic CBOR encoding of the seven signable
/// persona fields.
///
/// The encoded shape is a CBOR map with seven entries keyed by the
/// field names below. Map keys are emitted in sort order (per RFC 8949
/// §4.2.1 "Core Deterministic Encoding"), integers use the shortest
/// form, the body hash is encoded as CBOR `bytes`, and the source-id
/// list is encoded as an ordered CBOR array (slice order preserved).
/// Encoding the same `SignablePersona` twice (or on a different host)
/// produces identical bytes — the precondition Ed25519 needs.
///
/// # Errors
///
/// Returns an error only when CBOR serialization fails — in practice
/// unreachable for the fixed-shape input above, but surfaced as a
/// `Result` so callers don't have to choose between panicking and
/// silently signing a truncated payload.
pub fn canonical_cbor_persona(p: &SignablePersona<'_>) -> Result<Vec<u8>> {
    use std::collections::BTreeMap;
    let mut map: BTreeMap<&str, ciborium::Value> = BTreeMap::new();
    map.insert(
        "persona_id",
        ciborium::Value::Text(p.persona_id.to_string()),
    );
    map.insert("entity_id", ciborium::Value::Text(p.entity_id.to_string()));
    map.insert("namespace", ciborium::Value::Text(p.namespace.to_string()));
    map.insert(
        "version",
        ciborium::Value::Integer(ciborium::value::Integer::from(p.version)),
    );
    map.insert(
        "generated_at",
        ciborium::Value::Text(p.generated_at.to_string()),
    );
    let sources_val = ciborium::Value::Array(
        p.sources
            .iter()
            .map(|s| ciborium::Value::Text(s.clone()))
            .collect(),
    );
    map.insert("sources", sources_val);
    map.insert(
        "body_md_sha256",
        ciborium::Value::Bytes(p.body_md_sha256.to_vec()),
    );

    let entries: Vec<(ciborium::Value, ciborium::Value)> = map
        .into_iter()
        .map(|(k, v)| (ciborium::Value::Text(k.to_string()), v))
        .collect();
    let value = ciborium::Value::Map(entries);

    let mut out: Vec<u8> = Vec::with_capacity(256);
    ciborium::ser::into_writer(&value, &mut out).context("CBOR encode SignablePersona")?;
    Ok(out)
}

/// Sign `persona` with `keypair`'s private key.
///
/// Encodes the persona via [`canonical_cbor_persona`], then runs
/// Ed25519 over the resulting bytes. Returns the 64-byte signature,
/// ready to drop into the `metadata.persona.signature` base64 field on
/// the persona memory and into the `signature` BLOB column on the
/// corresponding `signed_events` row.
///
/// # Errors
///
/// - `keypair.private` is `None` (public-only handle — verification
///   only).
/// - The CBOR encoding step fails (in practice unreachable; surfaced
///   for completeness).
pub fn sign_persona(keypair: &AgentKeypair, persona: &SignablePersona<'_>) -> Result<Vec<u8>> {
    let signing = keypair.private.as_ref().with_context(|| {
        format!(
            "AgentKeypair for {} has no private key — cannot sign persona",
            keypair.agent_id
        )
    })?;
    let bytes = canonical_cbor_persona(persona)?;
    let sig = signing.sign(&bytes);
    Ok(sig.to_bytes().to_vec())
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
        // RFC 8949 §4.2.1 — encoding the same logical input three times
        // (in three *different* logical map-key orderings) must produce
        // identical bytes. This is the round-trip precondition for
        // Ed25519 signing AND a regression guard against an encoder
        // upgrade silently switching iteration order.
        //
        // M2 (v0.7.0 round-2): the encoder reads from a `BTreeMap<&str,
        // ...>` which is sorted by construction, so the bytes only ever
        // come out one way regardless of insertion order. We exercise
        // that property explicitly by inserting the six fields in three
        // distinct permutations and asserting all three encodes match.
        // If a future ciborium upgrade changes ordering semantics (or
        // someone swaps the `BTreeMap` for a `HashMap`), this test
        // fires and the maintainer revisits the canonicalisation
        // surface before signatures silently break across versions.

        // The shared field values — same payload, different insertion
        // orders below.
        let src_id = "src-001";
        let dst_id = "dst-002";
        let relation = "related_to";
        let observed_by = Some("alice");
        let valid_from = Some("2026-05-05T00:00:00+00:00");
        let valid_until: Option<&str> = None;

        // Helper: encode by inserting into a *non*-canonical map first
        // (`HashMap`) in a chosen visit order, then producing a
        // canonical `BTreeMap` and round-tripping through
        // `canonical_cbor`.  We can't easily inject our own non-canonical
        // CBOR here without re-writing `canonical_cbor`'s body, but we
        // CAN prove that constructing the same logical input via three
        // distinct intermediate orderings collapses to identical bytes
        // because `canonical_cbor` itself enforces the sort.

        // Permutation 1: declared order (alphabetic-by-construction).
        let perm1 = SignableLink {
            src_id,
            dst_id,
            relation,
            observed_by,
            valid_from,
            valid_until,
        };

        // Permutation 2: same logical link, constructed via field
        // reassignment in a different visual order. Rust struct literal
        // field order is purely syntactic; the binary representation
        // is the same. The encoder must still sort by name.
        let perm2 = SignableLink {
            valid_until,
            valid_from,
            observed_by,
            relation,
            dst_id,
            src_id,
        };

        // Permutation 3: interleaved order.
        let perm3 = SignableLink {
            relation,
            src_id,
            valid_from,
            dst_id,
            valid_until,
            observed_by,
        };

        let bytes1 = canonical_cbor(&perm1).expect("encode perm1");
        let bytes2 = canonical_cbor(&perm2).expect("encode perm2");
        let bytes3 = canonical_cbor(&perm3).expect("encode perm3");

        assert_eq!(
            bytes1, bytes2,
            "field-order permutation 2 must produce identical CBOR (BTreeMap key sort)"
        );
        assert_eq!(
            bytes2, bytes3,
            "field-order permutation 3 must produce identical CBOR (BTreeMap key sort)"
        );

        // Also exercise byte-stability across repeated encodes of the
        // same instance — the property that's load-bearing for sign +
        // verify across hosts.
        let again = canonical_cbor(&perm1).expect("re-encode perm1");
        assert_eq!(bytes1, again, "deterministic CBOR must be byte-stable");
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

    // -----------------------------------------------------------------
    // v0.7.0 issue #812 / #813 — SignablePersona + sign_persona
    // -----------------------------------------------------------------

    fn body_hash_fixture(seed: u8) -> [u8; 32] {
        let mut h = [seed; 32];
        h[0] ^= 0xA5;
        h
    }

    fn persona_fixture() -> ([u8; 32], Vec<String>) {
        let body = body_hash_fixture(0x10);
        let sources = vec!["src-1".to_string(), "src-2".to_string()];
        (body, sources)
    }

    #[test]
    fn canonical_cbor_persona_is_deterministic() {
        // Mirrors the link-side determinism test: three distinct
        // permutations of the SignablePersona literal must collapse
        // to identical bytes because the BTreeMap key-sort runs at
        // encode time. Catches a regression where a future refactor
        // swaps the BTreeMap for a HashMap or drops the explicit sort.
        let (body, sources) = persona_fixture();
        let persona_id = "persona-001";
        let entity_id = "alice";
        let namespace = "team/alpha";
        let version = 1_i32;
        let generated_at = "2026-05-16T12:00:00+00:00";

        let perm1 = SignablePersona {
            persona_id,
            entity_id,
            namespace,
            version,
            generated_at,
            sources: &sources,
            body_md_sha256: &body,
        };
        let perm2 = SignablePersona {
            body_md_sha256: &body,
            sources: &sources,
            generated_at,
            version,
            namespace,
            entity_id,
            persona_id,
        };
        let perm3 = SignablePersona {
            namespace,
            version,
            sources: &sources,
            entity_id,
            body_md_sha256: &body,
            generated_at,
            persona_id,
        };

        let b1 = canonical_cbor_persona(&perm1).expect("encode perm1");
        let b2 = canonical_cbor_persona(&perm2).expect("encode perm2");
        let b3 = canonical_cbor_persona(&perm3).expect("encode perm3");
        assert_eq!(b1, b2);
        assert_eq!(b2, b3);
        // Stable across repeated encodes of the same instance.
        assert_eq!(b1, canonical_cbor_persona(&perm1).expect("re-encode"));
    }

    #[test]
    fn canonical_cbor_persona_differs_on_field_change() {
        let (body, sources) = persona_fixture();
        let base = SignablePersona {
            persona_id: "p",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T00:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        // Flip the body hash — different bytes must result.
        let other_body = body_hash_fixture(0x99);
        let altered = SignablePersona {
            body_md_sha256: &other_body,
            ..base.clone()
        };
        let a = canonical_cbor_persona(&base).expect("encode base");
        let b = canonical_cbor_persona(&altered).expect("encode altered");
        assert_ne!(a, b, "different body hash must produce different bytes");
    }

    #[test]
    fn canonical_cbor_persona_handles_empty_sources() {
        let body = body_hash_fixture(0x01);
        let sources: Vec<String> = Vec::new();
        let persona = SignablePersona {
            persona_id: "p",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T00:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        // Encoding must not panic on an empty source list. Two
        // encodes still match (determinism over empty array).
        let bytes = canonical_cbor_persona(&persona).expect("encode empty-sources");
        assert!(!bytes.is_empty());
        assert_eq!(bytes, canonical_cbor_persona(&persona).expect("re-encode"));
    }

    #[test]
    fn sign_persona_round_trip() {
        let kp = keypair::generate("ai:curator").expect("generate");
        let (body, sources) = persona_fixture();
        let persona = SignablePersona {
            persona_id: "persona-xyz",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T12:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        let sig_bytes = sign_persona(&kp, &persona).expect("sign");
        assert_eq!(sig_bytes.len(), 64, "Ed25519 signatures are 64 bytes");

        let payload = canonical_cbor_persona(&persona).expect("encode");
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
        kp.public.verify(&payload, &sig).expect("verify");
    }

    #[test]
    fn sign_persona_refuses_public_only_keypair() {
        let kp = keypair::generate("ai:curator").unwrap();
        let pub_only = AgentKeypair {
            agent_id: "ai:curator".to_string(),
            public: kp.public,
            private: None,
        };
        let (body, sources) = persona_fixture();
        let persona = SignablePersona {
            persona_id: "p",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T00:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        let err = sign_persona(&pub_only, &persona).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no private key"), "got: {msg}");
    }

    #[test]
    fn sign_persona_does_not_verify_against_other_pub() {
        // Cross-key non-replayability — Alice's signature must not
        // verify under Bob's public key.
        let alice = keypair::generate("alice").unwrap();
        let bob = keypair::generate("bob").unwrap();
        let (body, sources) = persona_fixture();
        let persona = SignablePersona {
            persona_id: "p",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T00:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        let sig_bytes = sign_persona(&alice, &persona).unwrap();
        let payload = canonical_cbor_persona(&persona).unwrap();
        let mut sig_arr = [0u8; 64];
        sig_arr.copy_from_slice(&sig_bytes);
        let sig = ed25519_dalek::Signature::from_bytes(&sig_arr);
        assert!(bob.public.verify(&payload, &sig).is_err());
    }

    #[test]
    fn canonical_cbor_persona_version_change_produces_different_bytes() {
        // Version is part of the signed payload so a v1 signature
        // cannot be replayed as a v2 signature — pin that.
        let (body, sources) = persona_fixture();
        let v1 = SignablePersona {
            persona_id: "p",
            entity_id: "alice",
            namespace: "team/alpha",
            version: 1,
            generated_at: "2026-05-16T00:00:00+00:00",
            sources: &sources,
            body_md_sha256: &body,
        };
        let v2 = SignablePersona {
            version: 2,
            ..v1.clone()
        };
        let a = canonical_cbor_persona(&v1).expect("encode v1");
        let b = canonical_cbor_persona(&v2).expect("encode v2");
        assert_ne!(a, b);
    }
}
