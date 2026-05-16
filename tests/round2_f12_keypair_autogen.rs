// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Round-2 F12 — Ed25519 signing keypair auto-generated on first
//! `serve` startup.
//!
//! Pre-F12 the daemon refused to sign outbound links until the
//! operator manually ran `ai-memory identity generate`. v0.7.0 makes
//! the bootstrap path default-secure: a missing keypair triggers a
//! one-shot auto-gen at the resolved key path. The auto-gen must:
//!
//! 1. Create `<key_dir>/<agent_id>.{pub,priv}` when neither exists.
//! 2. Be idempotent — a second call must leave the existing keypair
//!    untouched (same bytes on disk).
//! 3. Skip when the operator has explicitly disabled identity in
//!    config (`disabled = true`).

use ai_memory::identity::keypair::{EnsureOutcome, ensure_keypair};

#[test]
fn auto_gen_creates_keypair_files_on_first_call() {
    let dir = tempfile::TempDir::new().unwrap();
    let outcome = ensure_keypair("daemon-test", dir.path(), false).expect("ensure");
    let pub_path = dir.path().join("daemon-test.pub");
    let priv_path = dir.path().join("daemon-test.priv");

    assert!(matches!(outcome, EnsureOutcome::Generated { .. }));
    assert!(pub_path.exists(), "public key must be written to disk");
    assert!(priv_path.exists(), "private key must be written to disk");

    // Files must be exactly 32 bytes (raw Ed25519 keys, no PEM/DER).
    let pub_bytes = std::fs::read(&pub_path).unwrap();
    let priv_bytes = std::fs::read(&priv_path).unwrap();
    assert_eq!(pub_bytes.len(), 32);
    assert_eq!(priv_bytes.len(), 32);
}

#[test]
fn auto_gen_is_idempotent_across_restarts() {
    let dir = tempfile::TempDir::new().unwrap();

    // First "serve startup" — generates a fresh keypair.
    let first = ensure_keypair("daemon-test", dir.path(), false).expect("first");
    assert!(matches!(first, EnsureOutcome::Generated { .. }));

    let pub_path = dir.path().join("daemon-test.pub");
    let priv_path = dir.path().join("daemon-test.priv");
    let pub_before = std::fs::read(&pub_path).unwrap();
    let priv_before = std::fs::read(&priv_path).unwrap();

    // Second "serve startup" — must observe the existing keypair and
    // NOT overwrite it. Overwriting would silently invalidate every
    // signed link the prior key produced.
    let second = ensure_keypair("daemon-test", dir.path(), false).expect("second");
    match second {
        EnsureOutcome::AlreadyExists { pub_path: observed } => {
            assert_eq!(observed, pub_path);
        }
        other => panic!("expected AlreadyExists on second call, got {other:?}"),
    }

    let pub_after = std::fs::read(&pub_path).unwrap();
    let priv_after = std::fs::read(&priv_path).unwrap();
    assert_eq!(pub_before, pub_after, "pub bytes must be preserved");
    assert_eq!(priv_before, priv_after, "priv bytes must be preserved");
}

#[test]
fn auto_gen_skips_when_identity_disabled() {
    let dir = tempfile::TempDir::new().unwrap();
    let outcome = ensure_keypair("daemon-test", dir.path(), true).expect("ensure");
    assert_eq!(outcome, EnsureOutcome::SkippedDisabled);

    // The filesystem must remain pristine — no keypair, no key dir
    // creation if it didn't exist.
    let pub_path = dir.path().join("daemon-test.pub");
    let priv_path = dir.path().join("daemon-test.priv");
    assert!(!pub_path.exists(), "must not write pub when disabled");
    assert!(!priv_path.exists(), "must not write priv when disabled");
}

#[test]
fn auto_gen_third_call_still_observes_existing() {
    // Defence-in-depth: idempotency holds across an arbitrary number
    // of calls, not just two.
    let dir = tempfile::TempDir::new().unwrap();
    let _ = ensure_keypair("daemon-test", dir.path(), false).unwrap();
    let _ = ensure_keypair("daemon-test", dir.path(), false).unwrap();
    let third = ensure_keypair("daemon-test", dir.path(), false).unwrap();
    assert!(matches!(third, EnsureOutcome::AlreadyExists { .. }));
}
