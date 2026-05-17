// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Shared test helpers for the governance integration suites.
//!
//! Six (currently seven) integration test files used to ship hand-rolled
//! copies of the same three helpers — `EnvVarGuard`, `install_test_operator_key`,
//! and `sign_rule` — totalling ~250 lines of cut-and-paste. Issue #821
//! consolidates those copies here.
//!
//! ## Usage
//!
//! Add to each integration test file (next to the other `use` lines at
//! the top, after the copyright header):
//!
//! ```ignore
//! mod common;
//! use common::*;
//! ```
//!
//! `cargo test` builds each `tests/*.rs` as a separate integration
//! binary; `tests/common/mod.rs` is a non-test module each binary pulls
//! in via the `mod common;` declaration. This is the canonical cargo
//! integration-test idiom (see the cargo book's
//! ["Submodules in integration tests"](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests)
//! section).
//!
//! Some helpers may be unused in a given binary (e.g. `sign_rule` is
//! only used by a subset of suites); the module-level
//! `#![allow(dead_code)]` below silences the per-binary
//! `dead_code` warnings that would otherwise fire.

#![allow(dead_code)]

use std::sync::Mutex;

use ai_memory::governance::rules_store::{self, Rule};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;

/// Process-wide lock that serializes env-var mutation across parallel
/// tests in the same integration binary. Each `EnvVarGuard` holds this
/// lock for its lifetime so a panicking assertion still restores prior
/// env state on unwind.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// RAII guard that sets an env var on construction and restores the
/// prior value (or unsets if previously unset) on drop. Holds the
/// process-wide `ENV_LOCK` for its lifetime so concurrent tests don't
/// race each other on the env mutation.
///
/// Use via [`EnvVarGuard::set`] — there is no public constructor that
/// bypasses the lock.
pub struct EnvVarGuard {
    key: &'static str,
    prev: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvVarGuard {
    /// Acquire the `ENV_LOCK`, snapshot the prior value of `key`, set
    /// `key` to `value`, return a guard that restores prior state on
    /// drop.
    pub fn set(key: &'static str, value: String) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var(key).ok();
        // SAFETY: env mutation is serialized by `ENV_LOCK` held in `_lock`.
        unsafe {
            std::env::set_var(key, value);
        }
        Self {
            key,
            prev,
            _lock: lock,
        }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        // SAFETY: env mutation is serialized by `ENV_LOCK` held in `_lock`.
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }
}

/// Generate a fresh test keypair and install its verifying key in the
/// `AI_MEMORY_OPERATOR_PUBKEY` env var so production
/// `resolve_operator_pubkey()` returns this key (bypasses the host's
/// on-disk `operator.key.pub`). Returns the signing key plus a guard
/// that restores the prior env var on drop.
pub fn install_test_operator_key() -> (SigningKey, EnvVarGuard) {
    let signing = SigningKey::generate(&mut OsRng);
    let pub_b64 =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signing.verifying_key().to_bytes());
    let guard = EnvVarGuard::set("AI_MEMORY_OPERATOR_PUBKEY", pub_b64);
    (signing, guard)
}

/// Build a rule, sign its canonical bytes with `signing`, and store the
/// 64-byte Ed25519 signature on the returned `Rule`. Mirrors what
/// `ai-memory rules sign-seed` produces in production. The rule's
/// `attest_level` is forced to `"operator_signed"` and any pre-existing
/// `signature` field is cleared before canonicalisation so the bytes
/// match the seed-loader's verify path.
pub fn sign_rule(mut rule: Rule, signing: &SigningKey) -> Rule {
    rule.attest_level = "operator_signed".into();
    rule.signature = None;
    let canonical =
        rules_store::canonical_bytes_for_signing(&rule).expect("canonical_bytes_for_signing");
    rule.signature = Some(signing.sign(&canonical).to_bytes().to_vec());
    rule
}
