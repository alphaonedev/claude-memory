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
//! integration-test idiom (see the cargo book's [Submodules in
//! integration tests](https://doc.rust-lang.org/cargo/reference/cargo-targets.html#integration-tests)
//! section).
//!
//! Some helpers may be unused in a given binary (e.g. `sign_rule` is
//! only used by a subset of suites); the module-level
//! `#![allow(dead_code)]` below silences the per-binary
//! `dead_code` warnings that would otherwise fire.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Mutex;

use ai_memory::governance::rules_store::{self, Rule};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use rand_core::OsRng;
use rusqlite::Connection;
use tempfile::NamedTempFile;

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

    /// Wave 2 Tier-A7 (issue #855) addition. Acquire the `ENV_LOCK`,
    /// snapshot the prior value of `key`, **remove** `key` from the
    /// process env, return a guard that restores prior state on drop.
    /// Used by `tests/config_precedence.rs` to exercise the
    /// "env unset → config wins over default" branch of the universal
    /// precedence ladder, which `set` (which requires a value) cannot
    /// express. Mirrors `set` so the lock + restore discipline is
    /// identical.
    pub fn remove(key: &'static str) -> Self {
        let lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var(key).ok();
        // SAFETY: env mutation is serialized by `ENV_LOCK` held in `_lock`.
        unsafe {
            std::env::remove_var(key);
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

// ---------------------------------------------------------------------
// Phase 2 helpers (issue #854) — five further high-duplication helpers
// pulled out of ~50 integration test files. See the commit body for
// the per-helper consolidation table.
// ---------------------------------------------------------------------

/// Read the `AI_MEMORY_TEST_POSTGRES_URL` env var, returning `None`
/// when unset. Every postgres-feature integration test gates its body
/// on the presence of this URL because the CI matrix runs both with
/// and without a live Postgres reachable. Was hand-rolled in ~20 test
/// files with bit-identical bodies before this consolidation.
#[must_use]
pub fn postgres_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_POSTGRES_URL").ok()
}

/// Read the `AI_MEMORY_TEST_AGE_URL` env var, returning `None` when
/// unset. Sibling of [`postgres_url`] for the Apache AGE-backed graph
/// tests. Mirrored here for symmetry with `postgres_url`.
#[must_use]
pub fn age_url() -> Option<String> {
    std::env::var("AI_MEMORY_TEST_AGE_URL").ok()
}

/// Pick an ephemeral 127.0.0.1 port by binding-and-dropping a
/// `TcpListener`. There is a TOCTOU window between drop and the next
/// bind, but it is acceptable for tests because the OS rarely hands
/// back the same port twice in quick succession on macOS / Linux, and
/// the alternative (hold the listener until the daemon binds) would
/// race the daemon's own bind. Was hand-rolled in ~21 daemon-spawning
/// test files; this helper standardises the one-liner shape.
#[must_use]
pub fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral 127.0.0.1:0");
    listener.local_addr().expect("local_addr").port()
}

/// Open a fresh `:memory:` `SQLite` connection through the production
/// `ai_memory::db::open` so migrations land before the test body runs.
/// Was hand-rolled in 9 capability/governance test files with
/// bit-identical bodies. Five further governance suites use a
/// hand-crafted `CREATE TABLE governance_rules + signed_events` batch
/// instead — those are intentionally NOT consolidated because they
/// probe schema-validation paths that must run independently of
/// `db::open`'s migration ladder.
#[must_use]
pub fn fresh_conn() -> Connection {
    ai_memory::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
}

/// `(NamedTempFile, PathBuf)` factory: create a tempfile, open the DB
/// once so migrations land, drop the connection so the caller can
/// re-open the path. The returned tempfile must be kept alive for the
/// duration of the test so its destructor doesn't unlink the DB out
/// from under the caller. Used by the K7/K8 webhook-and-quota suites
/// which pass the path repeatedly to `Connection::open` and MCP tool
/// handlers that take `&Path`.
#[must_use]
pub fn fresh_db_tempfile_path() -> (NamedTempFile, PathBuf) {
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

/// `(NamedTempFile, Connection)` factory: create a tempfile and open
/// the DB through `db::open`, keeping the connection live. Used by the
/// form-4/form-5/atomisation/wt1c suites that need both the live
/// connection and the tempfile guard.
#[must_use]
pub fn fresh_db_tempfile_conn() -> (NamedTempFile, Connection) {
    let tmp = NamedTempFile::new().expect("tempfile");
    let conn = ai_memory::db::open(tmp.path()).expect("db::open");
    (tmp, conn)
}
