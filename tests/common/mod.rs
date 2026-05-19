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

// ---------------------------------------------------------------------------
// v0.7.0 refactor PR-5 (#793) — shared K10 HMAC signing helper.
//
// The K10/K7 approval HTTP path binds a canonical request to a signature
// with the shape:
//
//     canonical = "<ts>.<METHOD>.<pending_id>.<body>"
//     sig       = HMAC-SHA256(sha256_hex(secret).as_bytes(), canonical)
//
// Six integration test files used to ship a hand-rolled copy of this
// helper (k10_approval_http, k10_approval_security, v070_a1_authn,
// serve_postgres_continuation2/3, l07_3_chunk_d_http_surface). The next
// canonical-bytes binding change (#791 v0.8.0 federation signed-signals)
// would have required 6+ identical edits. This helper consolidates the
// definition so future binding changes touch ONE site.
//
// Callers wrap the returned hex string in the `sha256=<hex>` envelope
// that the K10 verifier expects (or call [`sign_canonical_envelope`] for
// the wrapped form).
// ---------------------------------------------------------------------------

/// Hex-encode an SHA-256 hash of the supplied string. Used to derive
/// the HMAC key from the raw operator secret (matches the daemon-side
/// key-derivation in `src/handlers/...`).
#[must_use]
pub fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())
}

/// Decode a lowercase-hex string into bytes. Returns `None` for odd
/// length or non-hex characters.
#[must_use]
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Compute the HMAC-SHA256 of `body` keyed by `key_hex`. The key is
/// hex-decoded when possible (so callers can pass either a hex string
/// or a raw key); the function follows RFC 2104.
#[must_use]
pub fn hmac_sha256_hex(key_hex: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let key_bytes = hex_decode(key_hex).unwrap_or_else(|| key_hex.as_bytes().to_vec());
    let mut key = key_bytes;
    if key.len() > BLOCK {
        let mut h = Sha256::new();
        h.update(&key);
        key = h.finalize().to_vec();
    }
    key.resize(BLOCK, 0);
    let mut opad = [0x5cu8; BLOCK];
    let mut ipad = [0x36u8; BLOCK];
    for i in 0..BLOCK {
        opad[i] ^= key[i];
        ipad[i] ^= key[i];
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(body.as_bytes());
    let inner_digest = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner_digest);
    format!("{:x}", outer.finalize())
}

/// Compute the canonical K10 HMAC signature header value for an
/// approval request that binds `(timestamp, method, pending_id, body)`.
/// Returns the raw lowercase-hex digest (no `sha256=` prefix).
///
/// The binding shape:
///
/// ```text
/// canonical = "<timestamp>.<METHOD>.<pending_id>.<body>"
/// digest    = HMAC-SHA256(sha256_hex(secret), canonical)
/// ```
///
/// Use [`sign_canonical_envelope`] to obtain the `sha256=<hex>` envelope
/// the K10 verifier expects in the `X-Approval-Signature` header.
#[must_use]
pub fn sign_canonical(
    secret: &str,
    timestamp: &str,
    method: &str,
    pending_id: &str,
    body: &str,
) -> String {
    let key_hash = sha256_hex(secret);
    let canonical = format!("{timestamp}.{method}.{pending_id}.{body}");
    hmac_sha256_hex(&key_hash, &canonical)
}

/// Same as [`sign_canonical`] but wraps the digest in the
/// `sha256=<hex>` envelope the K10 verifier expects.
#[must_use]
pub fn sign_canonical_envelope(
    secret: &str,
    timestamp: &str,
    method: &str,
    pending_id: &str,
    body: &str,
) -> String {
    format!(
        "sha256={}",
        sign_canonical(secret, timestamp, method, pending_id, body)
    )
}

#[cfg(test)]
mod hmac_fixture_tests {
    use super::{sign_canonical, sign_canonical_envelope};

    /// Pin the canonical-bytes shape so a future binding-change PR is
    /// loud. If this assert fires, every K10 client (including any
    /// out-of-tree integration) needs to update.
    #[test]
    fn sign_canonical_binds_method_and_pending_id() {
        let a = sign_canonical("secret", "1700000000", "POST", "pa_123", "{}");
        let b = sign_canonical("secret", "1700000000", "POST", "pa_124", "{}");
        let c = sign_canonical("secret", "1700000000", "DELETE", "pa_123", "{}");
        assert_ne!(a, b, "pending_id MUST be in the canonical bytes");
        assert_ne!(a, c, "method MUST be in the canonical bytes");
    }

    /// The envelope shape is `sha256=<lowercase-hex>` so the K10
    /// verifier can split on `=` and pick the algorithm tag.
    #[test]
    fn sign_canonical_envelope_uses_sha256_prefix() {
        let env = sign_canonical_envelope("secret", "1700000000", "POST", "pa_1", "{}");
        assert!(env.starts_with("sha256="), "envelope: {env}");
        let digest = env.trim_start_matches("sha256=");
        assert_eq!(
            digest.len(),
            64,
            "SHA-256 digest is 32 bytes = 64 hex chars"
        );
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
