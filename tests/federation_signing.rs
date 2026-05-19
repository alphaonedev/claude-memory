// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 #791 — federation per-message Ed25519 signing regression test.
//!
//! Pins the three contract arms the receiver enforces:
//!
//! 1. signed body with matching key → `verify_header` returns Ok
//! 2. unsigned body (no header) → `VerifyError::Missing`
//! 3. tampered body OR wrong key → `VerifyError::BadSignature`
//!
//! These map 1:1 to the HTTP 200 / 401 / 401 contract the receiver
//! enforces in `handlers/federation_receive::sync_push`. The unit
//! coverage in `src/federation/signing.rs` already exercises the
//! algorithm-tag and base64-decode arms; this file is the
//! integration-level guard that the wire shape stays stable.

use ai_memory::federation::signing as fed_signing;
use ed25519_dalek::SigningKey;
use rand_core::OsRng;

fn fresh_key() -> SigningKey {
    SigningKey::generate(&mut OsRng)
}

#[test]
fn signed_message_verifies_at_receiver() {
    let signer = fresh_key();
    let pubkey = signer.verifying_key();
    let body = br#"{"sender_agent_id":"ai:node-1","memories":[]}"#;
    let header = fed_signing::sign_body_header(&signer, body);
    assert!(
        header.starts_with(fed_signing::ED25519_PREFIX),
        "header must use `ed25519=` algorithm prefix, got: {header}"
    );
    fed_signing::verify_header(Some(&header), body, &pubkey)
        .expect("signed body must verify under matching pubkey");
}

#[test]
fn unsigned_message_is_rejected_when_required() {
    let signer = fresh_key();
    let pubkey = signer.verifying_key();
    let body = br#"{"sender_agent_id":"ai:node-1","memories":[]}"#;
    let err = fed_signing::verify_header(None, body, &pubkey).unwrap_err();
    assert!(matches!(err, fed_signing::VerifyError::Missing));
    assert_eq!(err.tag(), "x_memory_sig_missing");
}

#[test]
fn tampered_message_fails_verification() {
    let signer = fresh_key();
    let pubkey = signer.verifying_key();
    let body = br#"{"sender_agent_id":"ai:node-1","memories":[{"id":"a"}]}"#;
    let header = fed_signing::sign_body_header(&signer, body);
    let tampered = br#"{"sender_agent_id":"ai:node-1","memories":[{"id":"EVIL"}]}"#;
    let err = fed_signing::verify_header(Some(&header), tampered, &pubkey).unwrap_err();
    assert!(matches!(err, fed_signing::VerifyError::BadSignature));
    assert_eq!(err.tag(), "x_memory_sig_bad_signature");
}

#[test]
fn wrong_key_fails_verification() {
    let signer = fresh_key();
    let other_pubkey = fresh_key().verifying_key();
    let body = br#"{"sender_agent_id":"ai:node-1"}"#;
    let header = fed_signing::sign_body_header(&signer, body);
    let err = fed_signing::verify_header(Some(&header), body, &other_pubkey).unwrap_err();
    assert!(matches!(err, fed_signing::VerifyError::BadSignature));
}

#[test]
fn require_sig_env_default_is_strict() {
    // The receiver's enforcement gate defaults to ON (v0.7.0 secure
    // default). Operators who haven't enrolled peer keys yet flip
    // `AI_MEMORY_FED_REQUIRE_SIG=0` during enrolment.
    // SAFETY: env mutation; scrubbed before.
    unsafe {
        std::env::remove_var(fed_signing::REQUIRE_SIG_ENV);
    }
    assert!(
        fed_signing::require_sig(),
        "v0.7.0 secure default must be ON"
    );
}
