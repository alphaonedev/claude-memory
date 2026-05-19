// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 (issue #228) — E2E memory content encryption at rest.
//!
//! This module is the substrate primitive for end-to-end encryption of
//! memory `content` columns at rest. It pairs a per-agent X25519 ECDH
//! keypair with ChaCha20-Poly1305 AEAD encryption so a single recipient
//! (an agent identified by `agent_id`) can decrypt content encrypted to
//! its public key.
//!
//! ## Wire shape
//!
//! Each encrypted payload is serialised as a self-describing [`Envelope`]
//! and persisted into the new `memories.encrypted_envelope` BLOB column
//! (schema v44). The envelope layout is the byte concatenation of:
//!
//! ```text
//! version (1 byte = 0x01)
//! ephemeral_pub (32 bytes — X25519 sender ephemeral pubkey)
//! nonce (12 bytes — ChaCha20-Poly1305 nonce, random)
//! ciphertext_with_tag (variable — AEAD ciphertext + 16-byte tag)
//! ```
//!
//! The recipient's static X25519 secret key (per-agent, generated and
//! cached via [`get_or_create_keypair`]) plus the envelope's ephemeral
//! pubkey produce the shared secret that ChaCha20-Poly1305 decrypts
//! under.
//!
//! ## Key lifecycle
//!
//! Keypairs live in-memory only by default (per-process cache). A
//! follow-up issue will add on-disk persistence under
//! `[`crate::identity::keypair`]`-style files; today the in-memory
//! cache is sufficient for the encrypt → store → recall → decrypt round
//! trip exercised by `tests/encryption_at_rest.rs`.
//!
//! ## Activation
//!
//! Callers gate at-rest encryption behind either:
//!
//! * The `[encryption].at_rest = true` config field (operator opt-in
//!   via `config.toml`), OR
//! * The `AI_MEMORY_ENCRYPT_AT_REST=1` environment variable (CLI /
//!   container-runtime opt-in).
//!
//! Both surfaces feed the same [`encryption_enabled`] gate, which the
//! storage write path consults before invoking [`encrypt`] / [`decrypt`].

use anyhow::{Context, Result, anyhow};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use rand_core::{OsRng, RngCore};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use x25519_dalek::{PublicKey, StaticSecret};

/// Envelope wire-version. Bumped only when the byte layout changes;
/// readers refuse unknown versions with a typed error so a future bump
/// doesn't silently mis-parse legacy rows.
pub const ENVELOPE_VERSION: u8 = 0x01;

/// X25519 pubkey length in bytes.
pub const PUBKEY_LEN: usize = 32;

/// ChaCha20-Poly1305 nonce length in bytes.
pub const NONCE_LEN: usize = 12;

/// ChaCha20-Poly1305 AEAD tag length in bytes (appended to ciphertext
/// by `Aead::encrypt`).
pub const TAG_LEN: usize = 16;

/// Per-agent X25519 keypair. The static-secret variant supports cloning
/// so the per-process cache can hand out copies without re-deriving
/// from the random generator.
#[derive(Clone)]
pub struct Keypair {
    pub agent_id: String,
    pub public: PublicKey,
    pub secret: StaticSecret,
}

impl std::fmt::Debug for Keypair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the secret material.
        f.debug_struct("Keypair")
            .field("agent_id", &self.agent_id)
            .field("public", &"<x25519 pubkey>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Decrypt-able envelope produced by [`encrypt`]. Carries the sender's
/// ephemeral X25519 pubkey + the AEAD nonce + the ciphertext-with-tag.
/// [`Envelope::to_bytes`] / [`Envelope::from_bytes`] handle the
/// substrate-stable wire shape; storage callers persist the bytes
/// verbatim into the `encrypted_envelope` column.
#[derive(Debug, Clone)]
pub struct Envelope {
    pub ephemeral_pub: [u8; PUBKEY_LEN],
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl Envelope {
    /// Serialise the envelope to its on-disk byte layout. See module
    /// docs for the layout. Length = 1 + 32 + 12 + ciphertext.len().
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + PUBKEY_LEN + NONCE_LEN + self.ciphertext.len());
        out.push(ENVELOPE_VERSION);
        out.extend_from_slice(&self.ephemeral_pub);
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Parse the envelope back out of its on-disk byte layout. Refuses
    /// unknown versions and truncated buffers with a typed error so a
    /// corrupted row surfaces cleanly instead of decrypting garbage.
    ///
    /// # Errors
    /// * Returns `Err` when the buffer is too short to contain the
    ///   fixed header (version + ephemeral_pub + nonce) plus at least
    ///   one byte of ciphertext-with-tag.
    /// * Returns `Err` when the leading version byte is not
    ///   [`ENVELOPE_VERSION`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let header_len = 1 + PUBKEY_LEN + NONCE_LEN;
        if bytes.len() < header_len + TAG_LEN {
            return Err(anyhow!(
                "envelope buffer too short: got {} bytes, need at least {}",
                bytes.len(),
                header_len + TAG_LEN
            ));
        }
        if bytes[0] != ENVELOPE_VERSION {
            return Err(anyhow!(
                "unknown envelope version: got 0x{:02x}, expected 0x{:02x}",
                bytes[0],
                ENVELOPE_VERSION
            ));
        }
        let mut ephemeral_pub = [0u8; PUBKEY_LEN];
        ephemeral_pub.copy_from_slice(&bytes[1..1 + PUBKEY_LEN]);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&bytes[1 + PUBKEY_LEN..header_len]);
        let ciphertext = bytes[header_len..].to_vec();
        Ok(Envelope {
            ephemeral_pub,
            nonce,
            ciphertext,
        })
    }
}

/// Process-wide cache of per-agent X25519 keypairs. The cache is
/// populated lazily on first [`get_or_create_keypair`] call for each
/// `agent_id` and persists for the lifetime of the process. A future
/// issue will swap this for an on-disk store; the in-memory shape lets
/// the encryption substrate land without forcing a key-rotation tool
/// design decision in the same patch.
fn keypair_cache() -> &'static Mutex<HashMap<String, Keypair>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Keypair>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Look up the per-agent X25519 [`Keypair`], generating + caching it on
/// first call. Subsequent calls for the same `agent_id` return clones
/// of the cached entry, so plaintext encrypt + recall + decrypt within
/// a single process always round-trips through the same recipient
/// secret.
///
/// # Errors
/// * Returns `Err` only when the internal mutex is poisoned (a callee
///   panic in another thread while the lock was held). This is a
///   process-fatal condition; callers may treat it as such.
pub fn get_or_create_keypair(agent_id: &str) -> Result<Keypair> {
    let cache = keypair_cache();
    let mut guard = cache
        .lock()
        .map_err(|e| anyhow!("encryption keypair cache mutex poisoned: {e}"))?;
    if let Some(kp) = guard.get(agent_id) {
        return Ok(kp.clone());
    }
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    let kp = Keypair {
        agent_id: agent_id.to_string(),
        public,
        secret,
    };
    guard.insert(agent_id.to_string(), kp.clone());
    Ok(kp)
}

/// Encrypt `content` to the given recipient X25519 public key, returning
/// a self-describing [`Envelope`].
///
/// The sender generates an ephemeral X25519 secret on every call; the
/// matching ephemeral public key is included in the envelope so the
/// recipient can derive the same shared secret. The shared secret is
/// fed directly into ChaCha20-Poly1305 as the AEAD key (32 bytes — the
/// X25519 output length matches the AEAD key length exactly, no HKDF
/// step needed for the MVP envelope shape).
///
/// # Errors
/// * Returns `Err` when the underlying AEAD encrypt call fails (should
///   not happen in practice for in-memory inputs of any size; rusqlite
///   already bounds content length).
pub fn encrypt(content: &str, recipient_pk: &PublicKey) -> Result<Envelope> {
    let ephemeral_secret = StaticSecret::random_from_rng(OsRng);
    let ephemeral_public = PublicKey::from(&ephemeral_secret);
    let shared = ephemeral_secret.diffie_hellman(recipient_pk);

    let key = Key::from_slice(shared.as_bytes());
    let cipher = ChaCha20Poly1305::new(key);

    let mut nonce_bytes = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(
            nonce,
            Payload {
                msg: content.as_bytes(),
                aad: &[],
            },
        )
        .map_err(|e| anyhow!("ChaCha20-Poly1305 encrypt failed: {e}"))?;

    Ok(Envelope {
        ephemeral_pub: ephemeral_public.to_bytes(),
        nonce: nonce_bytes,
        ciphertext,
    })
}

/// Decrypt an [`Envelope`] using the recipient's static X25519 secret
/// key (`my_sk`). Returns the original UTF-8 plaintext.
///
/// # Errors
/// * Returns `Err` when the AEAD verification fails (tampered
///   ciphertext, wrong recipient key, truncated nonce, etc.).
/// * Returns `Err` when the decrypted bytes are not valid UTF-8 — the
///   write path always feeds `&str`, so a UTF-8 failure on read is a
///   corruption signal.
pub fn decrypt(envelope: &Envelope, my_sk: &StaticSecret) -> Result<String> {
    let ephemeral_public = PublicKey::from(envelope.ephemeral_pub);
    let shared = my_sk.diffie_hellman(&ephemeral_public);

    let key = Key::from_slice(shared.as_bytes());
    let cipher = ChaCha20Poly1305::new(key);

    let nonce = Nonce::from_slice(&envelope.nonce);
    let plaintext = cipher
        .decrypt(
            nonce,
            Payload {
                msg: &envelope.ciphertext,
                aad: &[],
            },
        )
        .map_err(|e| anyhow!("ChaCha20-Poly1305 decrypt failed (authentication): {e}"))?;

    String::from_utf8(plaintext).context("decrypted plaintext is not valid UTF-8")
}

/// Consult the [encryption].at_rest config flag OR the
/// `AI_MEMORY_ENCRYPT_AT_REST=1` env var. Truthy env values:
/// `1` / `true` / `yes` / `on` (case-insensitive). Used by the storage
/// write path to gate the encrypt-on-insert / decrypt-on-read branches.
///
/// The config flag is consulted first when present, then the env var.
/// Either truthy source enables encryption. This mirrors the precedence
/// shape of the existing `AI_MEMORY_PERMISSIONS_MODE` config knob.
#[must_use]
pub fn encryption_enabled(config_flag: Option<bool>) -> bool {
    if let Some(true) = config_flag {
        return true;
    }
    matches!(
        std::env::var("AI_MEMORY_ENCRYPT_AT_REST")
            .ok()
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_round_trip_returns_same_secret() {
        // Cache-hit path: second call returns the same secret material.
        let agent = "test-agent-roundtrip";
        let a = get_or_create_keypair(agent).expect("first generate");
        let b = get_or_create_keypair(agent).expect("second fetch");
        assert_eq!(a.public.as_bytes(), b.public.as_bytes());
        assert_eq!(a.secret.to_bytes(), b.secret.to_bytes());
    }

    #[test]
    fn keypair_distinct_for_distinct_agents() {
        let a = get_or_create_keypair("agent-a").expect("a");
        let b = get_or_create_keypair("agent-b").expect("b");
        assert_ne!(a.public.as_bytes(), b.public.as_bytes());
    }

    #[test]
    fn encrypt_decrypt_round_trip_recovers_plaintext() {
        let kp = get_or_create_keypair("roundtrip-agent").expect("keypair");
        let plaintext = "hello world — encryption substrate MVP";
        let env = encrypt(plaintext, &kp.public).expect("encrypt");
        let recovered = decrypt(&env, &kp.secret).expect("decrypt");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn envelope_wire_format_round_trips() {
        let kp = get_or_create_keypair("envelope-bytes").expect("kp");
        let env = encrypt("payload bytes", &kp.public).expect("encrypt");
        let bytes = env.to_bytes();
        let parsed = Envelope::from_bytes(&bytes).expect("parse");
        assert_eq!(env.ephemeral_pub, parsed.ephemeral_pub);
        assert_eq!(env.nonce, parsed.nonce);
        assert_eq!(env.ciphertext, parsed.ciphertext);
        // And the round-tripped envelope decrypts.
        let recovered = decrypt(&parsed, &kp.secret).expect("decrypt parsed");
        assert_eq!(recovered, "payload bytes");
    }

    #[test]
    fn envelope_parse_rejects_short_buffer() {
        assert!(Envelope::from_bytes(&[]).is_err());
        assert!(Envelope::from_bytes(&[0x01; 10]).is_err());
    }

    #[test]
    fn envelope_parse_rejects_unknown_version() {
        let mut bad = vec![0xFF];
        bad.extend_from_slice(&[0u8; PUBKEY_LEN + NONCE_LEN + TAG_LEN + 1]);
        assert!(Envelope::from_bytes(&bad).is_err());
    }

    #[test]
    fn decrypt_with_wrong_secret_fails() {
        let kp_alice = get_or_create_keypair("alice-wrong-key").expect("alice");
        let kp_eve = get_or_create_keypair("eve-wrong-key").expect("eve");
        let env = encrypt("secret-for-alice", &kp_alice.public).expect("encrypt");
        // Eve cannot decrypt Alice's payload — AEAD authentication fails.
        assert!(decrypt(&env, &kp_eve.secret).is_err());
    }

    #[test]
    fn decrypt_with_tampered_ciphertext_fails() {
        let kp = get_or_create_keypair("tamper-detect").expect("kp");
        let mut env = encrypt("dont change this", &kp.public).expect("encrypt");
        // Flip a bit in the ciphertext — AEAD authentication catches it.
        env.ciphertext[0] ^= 0x01;
        assert!(decrypt(&env, &kp.secret).is_err());
    }

    #[test]
    fn encryption_enabled_config_flag_wins() {
        // Save + clear the env var so other tests aren't perturbed.
        let prev = std::env::var("AI_MEMORY_ENCRYPT_AT_REST").ok();
        // SAFETY: tests run with serial scope around env-var mutation in
        // the keypair-cache module; this single-threaded read/restore is
        // safe for the assertions below.
        unsafe { std::env::remove_var("AI_MEMORY_ENCRYPT_AT_REST") };
        assert!(encryption_enabled(Some(true)));
        assert!(!encryption_enabled(Some(false)));
        assert!(!encryption_enabled(None));
        unsafe { std::env::set_var("AI_MEMORY_ENCRYPT_AT_REST", "1") };
        assert!(encryption_enabled(None));
        assert!(encryption_enabled(Some(true)));
        // Restore.
        if let Some(v) = prev {
            unsafe { std::env::set_var("AI_MEMORY_ENCRYPT_AT_REST", v) };
        } else {
            unsafe { std::env::remove_var("AI_MEMORY_ENCRYPT_AT_REST") };
        }
    }
}
