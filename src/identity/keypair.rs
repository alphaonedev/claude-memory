// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Per-agent Ed25519 keypair lifecycle (Track H, Task H1).
//!
//! This module is the OSS substrate for v0.7's "attested cortex" track.
//! Every agent that wants to sign outbound writes (links in H2, memories
//! in H3+, audit events in H5) needs a stable Ed25519 keypair. The four
//! verbs ([`generate`], [`save`], [`load`], [`list`]) plus the CLI
//! wrapper at [`crate::cli::identity`] are the entire OSS surface.
//!
//! # Storage layout
//!
//! Keys live under `<key_dir>/<agent_id>.{pub,priv}`:
//!
//! | File                  | Mode (Unix) | Contents                                    |
//! |-----------------------|-------------|---------------------------------------------|
//! | `<agent_id>.pub`      | `0o644`     | 32 raw bytes — `VerifyingKey::to_bytes()`   |
//! | `<agent_id>.priv`     | `0o600`     | 32 raw bytes — `SigningKey::to_bytes()`     |
//!
//! On Windows the mode bits do not apply; the files are created with
//! the inherited ACL of the parent directory. This is a known coverage
//! gap for the OSS layer — see "Hardware-backed key storage" below.
//!
//! The default key directory is `dirs::config_dir().join("ai-memory/keys/")`
//! on every platform (`~/.config/ai-memory/keys/` on Linux,
//! `~/Library/Application Support/ai-memory/keys/` on macOS,
//! `%APPDATA%\ai-memory\keys\` on Windows). The CLI will create it on
//! first use.
//!
//! # Hardware-backed key storage is OUT of OSS scope
//!
//! Per [`ROADMAP2.md`](../../../ROADMAP2.md) and
//! [`docs/v0.7/V0.7-EPIC.md`](../../../docs/v0.7/V0.7-EPIC.md), the
//! OSS path stops at file-based 0600 storage. TPM 2.0, PKCS#11 HSMs,
//! Apple Secure Enclave / TEE, AWS KMS / GCP KMS / Azure Key Vault
//! are intentionally **not** implemented in this crate. Operators who
//! need any of those should look at the **AgenticMem™** commercial
//! layer — same `AgentKeypair` shape, same wire format, hardware-backed
//! signing under the hood.
//!
//! The OSS code never imports a hardware-token library and never
//! depends on a non-pure-Rust dependency for key material. This is a
//! deliberate licensing + portability decision, not a "we'll get to it"
//! gap.
//!
//! # Format & interop
//!
//! - The on-disk format is the raw 32-byte key, no PEM, no DER, no
//!   header, no length prefix. This is the smallest possible shape
//!   that round-trips through `ed25519-dalek` and matches the COSE /
//!   CBOR wire format H2 will use.
//! - `export_pub` emits URL-safe, no-padding base64 of the public
//!   key bytes — short enough to paste into a Slack message or a
//!   peer's allowlist file.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{SigningKey, VerifyingKey};

use crate::validate;

/// Suffix for the public-key file (`<agent_id>.pub`).
const PUB_SUFFIX: &str = ".pub";
/// Suffix for the private-key file (`<agent_id>.priv`).
const PRIV_SUFFIX: &str = ".priv";

/// Length of an Ed25519 public key in bytes.
const PUBLIC_KEY_LEN: usize = ed25519_dalek::PUBLIC_KEY_LENGTH;
/// Length of an Ed25519 private/signing key seed in bytes.
const SECRET_KEY_LEN: usize = ed25519_dalek::SECRET_KEY_LENGTH;

/// Per-agent Ed25519 keypair.
///
/// `private` is `Option` because two of the lifecycle verbs ([`load`]
/// when no `.priv` exists and [`list`] which always skips private
/// material) yield a public-only handle. Code that needs to sign must
/// match on `private` and refuse with a clear error when missing.
#[derive(Debug, Clone)]
pub struct AgentKeypair {
    /// Logical agent identifier — same vocabulary as
    /// `crate::identity::resolve_agent_id`.
    pub agent_id: String,
    /// Public verifying key. Always loaded.
    pub public: VerifyingKey,
    /// Optional private signing key. `None` for public-only loads.
    pub private: Option<SigningKey>,
}

impl AgentKeypair {
    /// Returns `true` when the private key is present and the keypair
    /// can therefore sign.
    #[must_use]
    pub fn can_sign(&self) -> bool {
        self.private.is_some()
    }

    /// URL-safe, no-padding base64 encoding of the public key bytes.
    /// Stable wire format for `export-pub` and for peer allowlists.
    #[must_use]
    pub fn public_base64(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.public.to_bytes())
    }
}

/// Returns the default key storage directory:
/// `dirs::config_dir().join("ai-memory/keys/")`.
///
/// Errors when the OS does not advertise a config dir (extremely rare;
/// every supported target — Linux, macOS, Windows — returns one).
pub fn default_key_dir() -> Result<PathBuf> {
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow!("OS did not advertise a config directory for key storage"))?;
    Ok(base.join("ai-memory").join("keys"))
}

/// Generate a fresh Ed25519 keypair for `agent_id` using `OsRng`.
///
/// `agent_id` is validated against [`crate::validate::validate_agent_id`]
/// so callers cannot smuggle invalid characters into the on-disk filename.
pub fn generate(agent_id: &str) -> Result<AgentKeypair> {
    validate::validate_agent_id(agent_id)?;
    // ed25519-dalek 2.x consumes a `CryptoRngCore` (rand_core 0.6).
    // `OsRng` is the platform CSPRNG; it never blocks on modern OSes.
    let mut csprng = rand_core::OsRng;
    let private = SigningKey::generate(&mut csprng);
    let public = private.verifying_key();
    Ok(AgentKeypair {
        agent_id: agent_id.to_string(),
        public,
        private: Some(private),
    })
}

/// Persist `keypair` to `dir`.
///
/// Creates the directory tree (recursive `mkdir`) on first use. On
/// Unix the public file is written with mode `0o644` and the private
/// file with mode `0o600`. Both files are written atomically by the
/// underlying `fs::write` (single syscall on the modern OSes we
/// target — no temp-file rename dance because the file shape is fixed
/// 32 bytes and a partial write is recoverable by `generate` again).
///
/// Refuses if `keypair.private` is `None` — there is nothing to save
/// beyond a public key, and saving a public-only file is the job of
/// [`save_public_only`] (used by `import` when `--priv` is omitted).
pub fn save(keypair: &AgentKeypair, dir: &Path) -> Result<()> {
    let private = keypair.private.as_ref().ok_or_else(|| {
        anyhow!(
            "AgentKeypair for {} has no private key to save",
            keypair.agent_id
        )
    })?;
    fs::create_dir_all(dir).with_context(|| format!("creating key directory {}", dir.display()))?;

    let pub_path = dir.join(format!("{}{PUB_SUFFIX}", keypair.agent_id));
    let priv_path = dir.join(format!("{}{PRIV_SUFFIX}", keypair.agent_id));

    write_with_mode(&pub_path, &keypair.public.to_bytes(), 0o644)
        .with_context(|| format!("writing public key {}", pub_path.display()))?;
    write_with_mode(&priv_path, &private.to_bytes(), 0o600)
        .with_context(|| format!("writing private key {}", priv_path.display()))?;
    Ok(())
}

/// Persist only the public-key file. Used by `identity import` when the
/// caller supplies a public key without a private key (e.g., importing
/// a peer's allowlist entry). The corresponding `.priv` is left absent;
/// [`load`] will then return a public-only [`AgentKeypair`].
pub fn save_public_only(keypair: &AgentKeypair, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("creating key directory {}", dir.display()))?;
    let pub_path = dir.join(format!("{}{PUB_SUFFIX}", keypair.agent_id));
    write_with_mode(&pub_path, &keypair.public.to_bytes(), 0o644)
        .with_context(|| format!("writing public key {}", pub_path.display()))?;
    Ok(())
}

/// Load `agent_id`'s keypair from `dir`.
///
/// The public file must exist (errors otherwise). The private file is
/// optional — if absent the returned `AgentKeypair.private` is `None`
/// and the caller can verify but not sign.
pub fn load(agent_id: &str, dir: &Path) -> Result<AgentKeypair> {
    validate::validate_agent_id(agent_id)?;
    let pub_path = dir.join(format!("{agent_id}{PUB_SUFFIX}"));
    let priv_path = dir.join(format!("{agent_id}{PRIV_SUFFIX}"));

    let pub_bytes = fs::read(&pub_path)
        .with_context(|| format!("reading public key {}", pub_path.display()))?;
    if pub_bytes.len() != PUBLIC_KEY_LEN {
        bail!(
            "public key {} has {} bytes, expected {PUBLIC_KEY_LEN}",
            pub_path.display(),
            pub_bytes.len()
        );
    }
    let mut pub_arr = [0u8; PUBLIC_KEY_LEN];
    pub_arr.copy_from_slice(&pub_bytes);
    let public = VerifyingKey::from_bytes(&pub_arr)
        .with_context(|| format!("decoding public key {}", pub_path.display()))?;

    let private = match fs::read(&priv_path) {
        Ok(priv_bytes) => {
            if priv_bytes.len() != SECRET_KEY_LEN {
                bail!(
                    "private key {} has {} bytes, expected {SECRET_KEY_LEN}",
                    priv_path.display(),
                    priv_bytes.len()
                );
            }
            let mut priv_arr = [0u8; SECRET_KEY_LEN];
            priv_arr.copy_from_slice(&priv_bytes);
            let signing = SigningKey::from_bytes(&priv_arr);
            // Cross-check: the private key must derive the same public
            // key we just loaded. Mismatch means file tampering or a
            // stale .pub — refuse loudly rather than sign with the
            // wrong identity.
            if signing.verifying_key().to_bytes() != public.to_bytes() {
                bail!(
                    "private key {} does not match public key {}",
                    priv_path.display(),
                    pub_path.display()
                );
            }
            Some(signing)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow!(e))
                .with_context(|| format!("reading private key {}", priv_path.display()));
        }
    };

    Ok(AgentKeypair {
        agent_id: agent_id.to_string(),
        public,
        private,
    })
}

/// Enumerate every `<agent_id>.pub` under `dir` and return the
/// public-only keypairs. Private keys are **not** loaded — `list` is
/// the safe verb for ops dashboards and shell autocompletion.
///
/// Returns an empty `Vec` (not an error) when `dir` does not exist —
/// "no keys generated yet" is the common first-run state.
pub fn list(dir: &Path) -> Result<Vec<AgentKeypair>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in
        fs::read_dir(dir).with_context(|| format!("reading key directory {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let Some(stem) = name_str.strip_suffix(PUB_SUFFIX) else {
            continue;
        };
        // Skip .pub files whose stem is not a valid agent_id — they
        // can't have been written by this module's `save`.
        if validate::validate_agent_id(stem).is_err() {
            continue;
        }
        let path = entry.path();
        let pub_bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if pub_bytes.len() != PUBLIC_KEY_LEN {
            continue;
        }
        let mut pub_arr = [0u8; PUBLIC_KEY_LEN];
        pub_arr.copy_from_slice(&pub_bytes);
        let Ok(public) = VerifyingKey::from_bytes(&pub_arr) else {
            continue;
        };
        out.push(AgentKeypair {
            agent_id: stem.to_string(),
            public,
            private: None,
        });
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    Ok(out)
}

/// Decode a base64-encoded public key (URL-safe-no-pad **or** standard
/// padded) into a [`VerifyingKey`]. Used by `identity import` so
/// operators can paste either flavor of base64 they were sent.
pub fn decode_public_base64(s: &str) -> Result<VerifyingKey> {
    let trimmed = s.trim();
    let bytes = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(trimmed))
        .with_context(|| "decoding base64 public key".to_string())?;
    if bytes.len() != PUBLIC_KEY_LEN {
        bail!(
            "decoded public key has {} bytes, expected {PUBLIC_KEY_LEN}",
            bytes.len()
        );
    }
    let mut arr = [0u8; PUBLIC_KEY_LEN];
    arr.copy_from_slice(&bytes);
    VerifyingKey::from_bytes(&arr).with_context(|| "decoding public key bytes".to_string())
}

/// Read a 32-byte raw key file and return the bytes. Used by
/// `identity import` for `--pub <path> --priv <path>` when the operator
/// hands us files instead of base64. Errors loudly on a length mismatch.
pub fn read_raw_key_file(path: &Path) -> Result<[u8; SECRET_KEY_LEN]> {
    let bytes = fs::read(path).with_context(|| format!("reading key file {}", path.display()))?;
    if bytes.len() != SECRET_KEY_LEN {
        bail!(
            "key file {} has {} bytes, expected {SECRET_KEY_LEN}",
            path.display(),
            bytes.len()
        );
    }
    let mut arr = [0u8; SECRET_KEY_LEN];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

/// Cross-platform `fs::write` with an explicit Unix mode. On non-Unix
/// targets `mode` is ignored and the file inherits the parent ACL.
#[cfg(unix)]
fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    // Best-effort remove first so a previous, possibly stricter mode
    // on the same name doesn't block an `open` with `create_new`.
    let _ = fs::remove_file(path);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    use std::io::Write;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn write_with_mode(path: &Path, bytes: &[u8], _mode: u32) -> io::Result<()> {
    // Windows/non-Unix: mode bits don't apply. The file inherits the
    // parent directory ACL. Hardware-backed key storage on Windows is
    // out of OSS scope — see the AgenticMem commercial layer.
    fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::Signer;
    use ed25519_dalek::Verifier;
    use tempfile::TempDir;

    fn tmp_dir() -> TempDir {
        TempDir::new().expect("tempdir")
    }

    #[test]
    fn generate_yields_signing_keypair() {
        let kp = generate("alice").expect("generate");
        assert_eq!(kp.agent_id, "alice");
        assert!(
            kp.can_sign(),
            "freshly generated keypair must have private key"
        );
        // Public derives from private.
        let priv_pub = kp.private.as_ref().unwrap().verifying_key().to_bytes();
        assert_eq!(priv_pub, kp.public.to_bytes());
    }

    #[test]
    fn generate_rejects_invalid_agent_id() {
        assert!(generate("has space").is_err());
        assert!(generate("has\0null").is_err());
    }

    #[test]
    fn round_trip_save_then_load() {
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).expect("save");
        let loaded = load("alice", dir.path()).expect("load");
        assert_eq!(loaded.agent_id, "alice");
        assert_eq!(loaded.public.to_bytes(), kp.public.to_bytes());
        assert!(loaded.can_sign(), "private key should round-trip");
        // Sign with loaded key, verify with original public.
        let msg = b"hello world";
        let sig = loaded.private.as_ref().unwrap().sign(msg);
        assert!(kp.public.verify(msg, &sig).is_ok());
    }

    #[test]
    fn load_without_private_yields_public_only() {
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).expect("save");
        // Drop the private file.
        let priv_path = dir.path().join("alice.priv");
        fs::remove_file(&priv_path).expect("rm priv");
        let loaded = load("alice", dir.path()).expect("load");
        assert!(!loaded.can_sign(), "missing .priv must yield None private");
        assert_eq!(loaded.public.to_bytes(), kp.public.to_bytes());
    }

    #[cfg(unix)]
    #[test]
    fn save_writes_unix_mode_0600_and_0644() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).expect("save");

        let pub_meta = fs::metadata(dir.path().join("alice.pub")).unwrap();
        let priv_meta = fs::metadata(dir.path().join("alice.priv")).unwrap();

        // Mask off the file-type bits; we only care about the perm bits.
        let pub_mode = pub_meta.permissions().mode() & 0o777;
        let priv_mode = priv_meta.permissions().mode() & 0o777;
        assert_eq!(
            priv_mode, 0o600,
            "private key must be 0600, got {priv_mode:o}"
        );
        assert_eq!(pub_mode, 0o644, "public key must be 0644, got {pub_mode:o}");
    }

    #[test]
    fn list_enumerates_saved_keypairs() {
        let dir = tmp_dir();
        let alice = generate("alice").unwrap();
        let bob = generate("bob").unwrap();
        save(&alice, dir.path()).unwrap();
        save(&bob, dir.path()).unwrap();

        let listed = list(dir.path()).expect("list");
        assert_eq!(listed.len(), 2);
        // Sorted by agent_id.
        assert_eq!(listed[0].agent_id, "alice");
        assert_eq!(listed[1].agent_id, "bob");
        // No private keys in list output.
        for kp in &listed {
            assert!(!kp.can_sign(), "list must not load private keys");
        }
        // Public bytes match.
        assert_eq!(listed[0].public.to_bytes(), alice.public.to_bytes());
        assert_eq!(listed[1].public.to_bytes(), bob.public.to_bytes());
    }

    #[test]
    fn list_on_missing_dir_returns_empty() {
        let dir = tmp_dir();
        let nonexistent = dir.path().join("does-not-exist");
        let listed = list(&nonexistent).expect("list");
        assert!(listed.is_empty());
    }

    #[test]
    fn list_skips_unrelated_files() {
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        // Drop noise that should be skipped.
        fs::write(dir.path().join("README.txt"), b"ignore me").unwrap();
        fs::write(dir.path().join("not-a-key.pub"), b"too short").unwrap();

        let listed = list(dir.path()).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent_id, "alice");
    }

    #[test]
    fn load_rejects_truncated_public_key() {
        let dir = tmp_dir();
        fs::write(dir.path().join("alice.pub"), b"short").unwrap();
        let err = load("alice", dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected 32"), "got: {msg}");
    }

    #[test]
    fn load_rejects_priv_pub_mismatch() {
        let dir = tmp_dir();
        let alice = generate("alice").unwrap();
        let bob = generate("alice").unwrap();
        save(&alice, dir.path()).unwrap();
        // Overwrite .priv with a different keypair's private bytes.
        fs::remove_file(dir.path().join("alice.priv")).unwrap();
        // Use save_public_only path effectively: write a .priv that
        // doesn't match alice's .pub.
        let bob_priv = bob.private.as_ref().unwrap().to_bytes();
        write_with_mode(&dir.path().join("alice.priv"), &bob_priv, 0o600).unwrap();
        let err = load("alice", dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "got: {msg}");
    }

    #[test]
    fn export_pub_round_trips_through_base64() {
        let kp = generate("alice").unwrap();
        let b64 = kp.public_base64();
        let decoded = decode_public_base64(&b64).expect("decode");
        assert_eq!(decoded.to_bytes(), kp.public.to_bytes());
    }

    #[test]
    fn decode_public_base64_accepts_padded_form() {
        let kp = generate("alice").unwrap();
        let padded = base64::engine::general_purpose::STANDARD.encode(kp.public.to_bytes());
        let decoded = decode_public_base64(&padded).expect("decode padded");
        assert_eq!(decoded.to_bytes(), kp.public.to_bytes());
    }

    #[test]
    fn read_raw_key_file_validates_length() {
        let dir = tmp_dir();
        let p = dir.path().join("short.bin");
        fs::write(&p, b"short").unwrap();
        let err = read_raw_key_file(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected 32"), "got: {msg}");
    }

    #[test]
    fn save_refuses_public_only_keypair() {
        let dir = tmp_dir();
        let kp = AgentKeypair {
            agent_id: "alice".to_string(),
            public: generate("alice").unwrap().public,
            private: None,
        };
        let err = save(&kp, dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no private key to save"), "got: {msg}");
    }

    #[test]
    fn save_public_only_writes_pub_only() {
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        let pub_only = AgentKeypair {
            agent_id: "alice".to_string(),
            public: kp.public,
            private: None,
        };
        save_public_only(&pub_only, dir.path()).expect("save_public_only");
        assert!(dir.path().join("alice.pub").exists());
        assert!(!dir.path().join("alice.priv").exists());
        let loaded = load("alice", dir.path()).expect("load");
        assert!(!loaded.can_sign());
    }

    #[test]
    fn default_key_dir_ends_in_ai_memory_keys() {
        let p = default_key_dir().expect("default dir");
        let s = p.to_string_lossy();
        assert!(s.ends_with("ai-memory/keys") || s.ends_with("ai-memory\\keys"));
    }
}
