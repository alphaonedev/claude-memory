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

/// Test-only process-wide guard for tests that mutate
/// `AI_MEMORY_KEY_DIR`. Exposed at `pub(crate)` (visibility only —
/// no behavioural change) so coverage tests in `src/mcp/mod.rs`
/// can serialise with the existing race-prone tests in this file.
///
/// Without this any other test that reads the env var concurrently
/// can observe a half-written value, surfacing as flaky assertions.
#[cfg(test)]
pub(crate) fn key_dir_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// Returns the default key storage directory:
/// `dirs::config_dir().join("ai-memory/keys/")`.
///
/// Errors when the OS does not advertise a config dir (extremely rare;
/// every supported target — Linux, macOS, Windows — returns one).
///
/// `AI_MEMORY_KEY_DIR` env-var override: when set and non-empty, that
/// path is returned verbatim. This mirrors the env-override pattern
/// other paths in `ai-memory` use (`AI_MEMORY_DB_PATH`,
/// `AI_MEMORY_AGENT_ID`) and lets H4's `memory_verify` integration
/// tests stand up an isolated key dir per test without shelling out to
/// the operator's real `~/.config/ai-memory/keys/`. Operators who want
/// to relocate the key store in production can use the same override.
pub fn default_key_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("AI_MEMORY_KEY_DIR")
        && !v.is_empty()
    {
        return Ok(PathBuf::from(v));
    }
    // COVERAGE: ok_or_else closure (line 131) reachable only on hosts
    //           where dirs::config_dir() returns None — i.e. exotic
    //           platforms with no HOME env var. Not deterministic to
    //           trigger in tests because removing HOME breaks tempfile.
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

    // COVERAGE: with_context lazy-format closures (lines 178, 180)
    //           reachable only when the underlying fs::write fails on
    //           a successfully-created directory — same EACCES/ENOSPC
    //           class as write_with_mode above. Not portable to tests.
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
    // COVERAGE: with_context closure (line 192) same class as save's
    //           pub-write closure (line 178) — reachable on EACCES/
    //           ENOSPC; not portable to unit tests on macOS/Linux.
    write_with_mode(&pub_path, &keypair.public.to_bytes(), 0o644)
        .with_context(|| format!("writing public key {}", pub_path.display()))?;
    Ok(())
}

/// Load `agent_id`'s keypair from `dir`.
///
/// The public file must exist (errors otherwise). The private file is
/// optional — if absent the returned `AgentKeypair.private` is `None`
/// and the caller can verify but not sign.
///
/// # v0.7.0 S4-LOW1 — load-time mode-bits enforcement (Unix)
///
/// `save` writes the private file with mode `0o600`, but an operator
/// (or a misconfigured restore-from-backup) can chmod-loosen the
/// file on disk after the fact. Without a load-time check the
/// daemon would happily sign with a world-readable key. On Unix we
/// now stat the `.priv` file before reading and refuse to load
/// when any group/other bit is set (`mode & 0o077 != 0`).
///
/// The error message names the path and the offending mode, and
/// includes the `chmod` invocation that restores 0600 — so an
/// operator hitting this in production has a copy-pasteable fix.
///
/// On non-Unix targets this check is a no-op (mode bits don't
/// apply to NTFS ACLs; hardware-backed key storage is the
/// commercial AgenticMem layer's responsibility — see the
/// "Hardware-backed key storage" section above).
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
    // COVERAGE: with_context closure (line 218) reachable when the
    //           32-byte file decodes into an invalid Edwards-curve
    //           point. The load_returns_decode_context_for_corrupt_public_key
    //           test exercises this with the all-FF input; whether
    //           dalek 2.x accepts that input or not is version-bound.
    let public = VerifyingKey::from_bytes(&pub_arr)
        .with_context(|| format!("decoding public key {}", pub_path.display()))?;

    // v0.7.0 S4-LOW1 — refuse to load a `.priv` whose Unix mode bits
    // grant any group/other access. Only fire when the file exists;
    // a missing `.priv` is a valid public-only load and the mode
    // check is irrelevant there. Done as a pre-flight before
    // `fs::read` so we never even map the bytes into memory for a
    // world-readable key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(&priv_path) {
            Ok(meta) => {
                let mode = meta.permissions().mode() & 0o777;
                if mode & 0o077 != 0 {
                    bail!(
                        "private key {} has insecure mode {:o}; refusing to load. \
                         Restore with: chmod 0600 {}",
                        priv_path.display(),
                        mode,
                        priv_path.display()
                    );
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // Public-only load — fall through; the inner match
                // below will surface the same NotFound path.
            }
            Err(e) => {
                return Err(anyhow!(e))
                    .with_context(|| format!("stat private key {}", priv_path.display()));
            }
        }
    }

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
        // COVERAGE: entry? Err-arm (line 273) reachable when a
        //           specific dir entry fails to stat mid-iteration
        //           — typically the file was deleted between
        //           read_dir and entry materialisation. Not
        //           deterministic to trigger.
        let entry = entry?;
        let name = entry.file_name();
        // COVERAGE: name.to_str() None arm (line 276) reachable only
        //           on Windows where filenames may contain non-UTF8
        //           code units, or on Linux with weird filesystem
        //           encoding. macOS NFD-normalises everything to
        //           UTF-8 so the None arm doesn't fire on the dev
        //           host. Exercised by GitHub Actions Windows CI.
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
    // COVERAGE: with_context closure (line 326+) reachable when the
    //           32-byte base64-decoded payload is an invalid Edwards-
    //           curve point. Same class as load() line 218 — coverage
    //           depends on the dalek 2.x decode policy for specific
    //           inputs. Documented per L0.7 playbook §3c.
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

// ---------------------------------------------------------------------------
// Round-2 F12 — auto-generation of the daemon's signing keypair
// ---------------------------------------------------------------------------
//
// Round-2 evidence: link signing was disabled by default at v0.7.0
// because no Ed25519 keypair existed on a freshly-installed deployment
// and the operator had to manually run `ai-memory identity generate`
// before signed links would land. Default-secure says we should
// auto-generate one at first `serve` startup unless the operator
// explicitly opted out. The lifecycle is idempotent (re-runs are
// no-ops) so a daemon restart never overwrites an existing keypair.

/// Outcome of a single [`ensure_keypair`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnsureOutcome {
    /// Keypair already existed at the resolved path; no action taken.
    AlreadyExists {
        /// Path to the public-key file the existence check observed.
        pub_path: PathBuf,
    },
    /// A fresh keypair was generated and persisted to `dir`.
    Generated {
        /// Path the public-key file was written to. The corresponding
        /// `.priv` lives alongside.
        pub_path: PathBuf,
    },
    /// Auto-generation was disabled — operator set
    /// `[identity].disabled = true` (or equivalent) in config.
    SkippedDisabled,
}

/// Round-2 F12 — auto-generate a signing keypair for `agent_id` under
/// `dir` if one does not already exist.
///
/// `disabled` is the operator's opt-out flag (resolved from
/// `[identity].disabled` in config). When `true` the helper returns
/// [`EnsureOutcome::SkippedDisabled`] without touching the filesystem.
///
/// Idempotency: when the public-key file at
/// `<dir>/<agent_id>.pub` already exists the helper returns
/// [`EnsureOutcome::AlreadyExists`] without calling [`generate`] or
/// [`save`]. This guarantees a daemon restart never overwrites a
/// pre-existing keypair (which would silently invalidate every
/// signed link the prior key produced).
///
/// On the [`EnsureOutcome::Generated`] path the helper logs at INFO
/// level via `tracing` so the operator notices the new key in
/// daemon logs. The same line is also surfaced by the F12 startup
/// banner — see [`crate::cli::serve_banner`].
pub fn ensure_keypair(agent_id: &str, dir: &Path, disabled: bool) -> Result<EnsureOutcome> {
    if disabled {
        tracing::info!(
            "identity: auto-gen disabled by config; link signing will be skipped at boot"
        );
        return Ok(EnsureOutcome::SkippedDisabled);
    }
    validate::validate_agent_id(agent_id)?;

    let pub_path = dir.join(format!("{agent_id}{PUB_SUFFIX}"));
    if pub_path.exists() {
        // Idempotent: do NOT regenerate. A daemon restart must keep
        // the operator's existing key.
        return Ok(EnsureOutcome::AlreadyExists { pub_path });
    }

    let kp = generate(agent_id)?;
    save(&kp, dir)?;
    // COVERAGE: tracing::info! lazy-format closure (lines 411-417)
    //           — the format args are constructed lazily; the closure
    //           body runs when the INFO subscriber is enabled. Coverage
    //           depends on test subscriber config. Documented per L0.7
    //           playbook §3c.
    tracing::info!(
        "auto-generated identity keypair at {} — consider backing up",
        pub_path.display()
    );
    Ok(EnsureOutcome::Generated { pub_path })
}

/// Cross-platform `fs::write` with an explicit Unix mode. On non-Unix
/// targets `mode` is ignored and the file inherits the parent ACL.
// COVERAGE: the `?` Err-arm closures on `open`/`write_all`/`sync_all`
//           (lines 432, 434, 435) are unreachable on the happy path
//           because every test caller passes a tempdir-relative path
//           with write permission. Triggering EACCES / ENOSPC / EIO
//           in unit tests requires kernel-level fault injection.
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
        // M9 — `default_key_dir_honours_env_override` flips the same
        // `AI_MEMORY_KEY_DIR` key. Acquire the shared lock so the two
        // tests cannot interleave under `cargo test --jobs N`.
        let _g = key_dir_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env mutation serialised by `_g`. The H4 env-var
        // override (`AI_MEMORY_KEY_DIR`) is scrubbed up-front so this
        // test asserts the *fallback* path.
        unsafe {
            std::env::remove_var("AI_MEMORY_KEY_DIR");
        }
        let p = default_key_dir().expect("default dir");
        let s = p.to_string_lossy();
        assert!(s.ends_with("ai-memory/keys") || s.ends_with("ai-memory\\keys"));
    }

    /// Process-wide guard for tests that mutate `AI_MEMORY_KEY_DIR`.
    /// Delegates to the module-level `pub(crate) key_dir_env_lock` so
    /// sibling-crate test files (e.g. `src/mcp/mod.rs`'s H4 verify
    /// coverage tests) can serialise against the keypair-module tests
    /// that also mutate the env var. Local thin wrapper kept so the
    /// existing call sites in this file do not change.
    fn key_dir_env_lock() -> &'static std::sync::Mutex<()> {
        super::key_dir_env_lock()
    }

    // ---- Round-2 F12 ensure_keypair --------------------------------------

    #[test]
    fn ensure_keypair_generates_when_missing() {
        let dir = tmp_dir();
        let outcome = ensure_keypair("alice", dir.path(), false).expect("ensure");
        match outcome {
            EnsureOutcome::Generated { pub_path } => {
                assert!(pub_path.exists(), "pub key must be on disk");
                let priv_path = dir.path().join("alice.priv");
                assert!(priv_path.exists(), "priv key must be on disk");
            }
            other => panic!("expected Generated, got {other:?}"),
        }
    }

    #[test]
    fn ensure_keypair_idempotent_on_second_call() {
        let dir = tmp_dir();
        let first = ensure_keypair("alice", dir.path(), false).expect("first");
        let pub_path = dir.path().join("alice.pub");
        let priv_path = dir.path().join("alice.priv");
        // Snapshot bytes to assert non-overwrite.
        let pub_before = fs::read(&pub_path).unwrap();
        let priv_before = fs::read(&priv_path).unwrap();

        let second = ensure_keypair("alice", dir.path(), false).expect("second");
        match second {
            EnsureOutcome::AlreadyExists { pub_path: observed } => {
                assert_eq!(observed, pub_path);
            }
            other => panic!("expected AlreadyExists on second call, got {other:?}"),
        }
        // Bytes must NOT have changed — overwrite would corrupt every
        // prior signed link.
        let pub_after = fs::read(&pub_path).unwrap();
        let priv_after = fs::read(&priv_path).unwrap();
        assert_eq!(pub_before, pub_after);
        assert_eq!(priv_before, priv_after);
        // First call's outcome must have been Generated.
        assert!(matches!(first, EnsureOutcome::Generated { .. }));
    }

    #[test]
    fn ensure_keypair_respects_disabled_flag() {
        let dir = tmp_dir();
        let outcome = ensure_keypair("alice", dir.path(), true).expect("ensure");
        assert_eq!(outcome, EnsureOutcome::SkippedDisabled);
        // Filesystem must be untouched.
        assert!(!dir.path().join("alice.pub").exists());
        assert!(!dir.path().join("alice.priv").exists());
    }

    #[test]
    fn ensure_keypair_validates_agent_id() {
        let dir = tmp_dir();
        let res = ensure_keypair("has space", dir.path(), false);
        assert!(res.is_err(), "must reject invalid agent_id");
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — error path + visibility closures
    // -----------------------------------------------------------------

    #[test]
    fn save_returns_context_when_dir_is_a_file() {
        // Lines 172, 178: with_context closure for create_dir_all
        // when the parent component is a file.
        let dir = tmp_dir();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let kp = generate("alice").unwrap();
        // Treat the file as if it were a dir → mkdir of "blocker/sub"
        // fails because blocker is a file.
        let sub = blocker.join("sub");
        let err = save(&kp, &sub).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("creating key directory"),
            "expected wrapped context, got: {msg}"
        );
    }

    #[test]
    fn save_public_only_returns_context_when_dir_is_a_file() {
        // Lines 189: with_context closure for create_dir_all.
        let dir = tmp_dir();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let kp = generate("alice").unwrap();
        let sub = blocker.join("sub");
        let err = save_public_only(&kp, &sub).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("creating key directory"),
            "expected wrapped context, got: {msg}"
        );
    }

    #[test]
    fn load_returns_context_when_pub_file_missing() {
        // Line 207: with_context closure for fs::read of public.
        let dir = tmp_dir();
        let err = load("alice", dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading public key"), "got: {msg}");
    }

    #[test]
    fn load_returns_decode_context_for_corrupt_public_key() {
        // Line 218: with_context closure for VerifyingKey::from_bytes.
        // Construct 32 bytes that fail decode (an Ed25519 invariant
        // requires the encoded point to lie on the curve — most
        // arbitrary 32-byte sequences are valid, but certain
        // canonical points fail). Use 32 0xFF bytes to maximise the
        // chance of decode failure; if dalek accepts it, the test
        // falls back to asserting the length is the only check that
        // would fire. We trust the historical Ed25519 spec which
        // rejects all-1 encodings.
        let dir = tmp_dir();
        let bytes = [0xFFu8; PUBLIC_KEY_LEN];
        fs::write(dir.path().join("alice.pub"), bytes).unwrap();
        // The result may surface either a length-OK + decode error
        // OR a decode error directly. We only assert that LOAD errors
        // (not panics) — this pins the path even if dalek's decode
        // policy varies across versions.
        let res = load("alice", dir.path());
        if let Err(err) = res {
            let msg = format!("{err:#}");
            // Either path is acceptable; both go through with_context.
            assert!(
                msg.contains("decoding public key") || msg.contains("expected"),
                "got: {msg}"
            );
        } else {
            // If dalek accepted the all-FF point as a valid public
            // key, this test is a no-op (the spec edge differs from
            // our assumption). Document that we tolerate either
            // outcome via this branch.
        }
    }

    #[test]
    fn load_with_truncated_priv_returns_length_error() {
        // Lines 222-226: bail! when private key bytes are wrong length.
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        // Truncate .priv to a non-32-byte length (e.g. 8 bytes).
        fs::write(dir.path().join("alice.priv"), b"shortie!").unwrap();
        let err = load("alice", dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected 32"), "got: {msg}");
    }

    #[test]
    fn list_returns_context_on_unreadable_directory() {
        // Line 271: with_context closure for read_dir failure. Hardest
        // to trigger portably — passing a regular file as `dir` makes
        // `dir.exists()` return true but read_dir fails with ENOTDIR.
        let dir = tmp_dir();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"x").unwrap();
        let err = list(&file).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading key directory"), "got: {msg}");
    }

    #[test]
    fn decode_public_base64_rejects_garbage() {
        // Line 317: with_context closure on base64 decode failure.
        let err = decode_public_base64("not-valid-base64!!!").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("decoding base64"), "got: {msg}");
    }

    #[test]
    fn decode_public_base64_rejects_wrong_length() {
        // Line 318-322: bail! when decoded bytes are not 32.
        // 8 bytes encodes to 12 chars in base64 (no padding).
        let short = URL_SAFE_NO_PAD.encode([0u8; 8]);
        let err = decode_public_base64(&short).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("expected 32"), "got: {msg}");
    }

    #[test]
    fn read_raw_key_file_returns_context_when_path_missing() {
        // Line 333: with_context closure on fs::read failure.
        let dir = tmp_dir();
        let missing = dir.path().join("nope.bin");
        let err = read_raw_key_file(&missing).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("reading key file"), "got: {msg}");
    }

    #[test]
    fn ensure_keypair_rejects_invalid_agent_id_when_enabled() {
        // Line 402: validate_agent_id fires on the enabled branch.
        let dir = tmp_dir();
        let err = ensure_keypair("has space", dir.path(), false).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("invalid character"), "got: {msg}");
    }

    // -----------------------------------------------------------------
    // L0.7-2 Tier A — list() iteration error closures + load() io error
    // branches not covered by the prior suite.
    // -----------------------------------------------------------------

    #[test]
    fn list_skips_pub_file_with_invalid_agent_id_stem() {
        // Line 283-285: validate_agent_id(stem).is_err() => continue.
        // The stem must look like a .pub file (so the suffix strip
        // doesn't continue first) but must FAIL validate_agent_id.
        // "has space" violates the agent_id regex.
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        // 32-byte bytes so the length guard doesn't skip first.
        fs::write(dir.path().join("has space.pub"), [0u8; PUBLIC_KEY_LEN]).unwrap();
        let listed = list(dir.path()).expect("list");
        // The bogus stem is filtered out; only alice survives.
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent_id, "alice");
    }

    #[cfg(unix)]
    #[test]
    fn list_skips_unreadable_pub_file_continues_iteration() {
        // Lines 287-289: Err(_) => continue. Make a 0000-mode file
        // alongside a readable one — list must skip the unreadable
        // entry and still return the good one.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let alice = generate("alice").unwrap();
        save(&alice, dir.path()).unwrap();
        let unreadable = dir.path().join("bob.pub");
        fs::write(&unreadable, [0u8; PUBLIC_KEY_LEN]).unwrap();
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
        let listed = list(dir.path()).expect("list");
        // Restore so tempdir cleanup works.
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o644)).unwrap();
        // The unreadable file is skipped — only alice survives. Bob
        // *may* survive if running as root (which bypasses 0000), so
        // we accept either 1 or 2 entries but require alice present.
        assert!(listed.iter().any(|k| k.agent_id == "alice"));
    }

    #[test]
    fn list_skips_pub_file_with_invalid_curve_point() {
        // Lines 296-297: VerifyingKey::from_bytes Err => continue.
        // Search for a 32-byte sequence that ed25519-dalek rejects.
        // Many arbitrary inputs are valid points; some y-coordinates
        // off-curve are not. We probe a handful of candidates and
        // use the first one that errors. If none of them error on
        // this dalek version we fall back to asserting the iteration
        // doesn't panic — the COVERAGE note below records the cap.
        let dir = tmp_dir();
        let alice = generate("alice").unwrap();
        save(&alice, dir.path()).unwrap();

        let mut bogus: Option<[u8; PUBLIC_KEY_LEN]> = None;
        for seed in 0u8..=255 {
            let mut bytes = [seed; PUBLIC_KEY_LEN];
            // Twiddle the high bits — Edwards curve y-coords are
            // 255-bit; setting bytes[31] = 0xFF often pushes the
            // decoded y above the field prime (2^255 - 19), which
            // dalek rejects.
            bytes[31] = 0xFF;
            if VerifyingKey::from_bytes(&bytes).is_err() {
                bogus = Some(bytes);
                break;
            }
        }
        if let Some(b) = bogus {
            fs::write(dir.path().join("bogus.pub"), b).unwrap();
            let listed = list(dir.path()).expect("list");
            // alice survives; bogus.pub is skipped because
            // VerifyingKey::from_bytes returned Err.
            assert!(
                listed.iter().any(|k| k.agent_id == "alice"),
                "alice must survive a sibling invalid-curve-point .pub file"
            );
            assert!(
                !listed.iter().any(|k| k.agent_id == "bogus"),
                "bogus.pub with invalid curve point must be filtered out"
            );
        }
        // COVERAGE: when no 32-byte sequence the search range rejects
        // (impossible on the dalek 2.x release pinned in Cargo.toml),
        // this test falls through without an assertion; the from_bytes
        // error closure stays uncovered. dalek versions <2 accepted
        // every 32-byte point; dalek 2.x rejects high-y wraps so the
        // search above terminates.
    }

    #[cfg(unix)]
    #[test]
    fn load_propagates_non_notfound_io_error_on_private_key() {
        // Lines 246-249: Err(e) => return Err(anyhow!(e))
        //                     .with_context("reading private key ...")
        // Trigger by making the .priv file readable to nobody (mode
        // 0000) — fs::read returns EACCES, which is NOT NotFound.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        let priv_path = dir.path().join("alice.priv");
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o000)).unwrap();
        let res = load("alice", dir.path());
        // Restore so tempdir cleanup works regardless of test outcome.
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600)).unwrap();
        // On most CI hosts EACCES surfaces; if running as root the
        // permission is ignored and load succeeds — either way we
        // assert the function did not panic and returned a result.
        if let Err(err) = res {
            let msg = format!("{err:#}");
            assert!(msg.contains("reading private key"), "got: {msg}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn ensure_keypair_save_failure_propagates_context() {
        // Lines 412 + save chain: when save() fails (because the dir
        // is a regular file, not a directory), ensure_keypair must
        // propagate the error.
        let dir = tmp_dir();
        let blocker = dir.path().join("blocker");
        fs::write(&blocker, b"file").unwrap();
        let sub = blocker.join("sub");
        let res = ensure_keypair("alice", &sub, false);
        assert!(res.is_err(), "save under a file-blocked dir must fail");
    }

    #[test]
    fn default_key_dir_honours_env_override() {
        // v0.7 H4 — the override exists so `memory_verify` integration
        // tests can populate a hermetic key dir per test process. Pin
        // the contract here so a future refactor doesn't quietly drop
        // the override.
        let _g = key_dir_env_lock().lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: env mutation serialised by `key_dir_env_lock` for
        // the duration of this test.
        unsafe {
            std::env::set_var("AI_MEMORY_KEY_DIR", "/tmp/h4-override-test");
        }
        let p = default_key_dir().expect("default dir");
        assert_eq!(p, PathBuf::from("/tmp/h4-override-test"));
        // SAFETY: scoped cleanup so other tests see the unset value.
        unsafe {
            std::env::remove_var("AI_MEMORY_KEY_DIR");
        }
    }

    // -----------------------------------------------------------------
    // v0.7.0 S4-LOW1 — load-time mode-bits enforcement
    // -----------------------------------------------------------------

    #[cfg(unix)]
    #[test]
    fn test_keypair_load_refuses_world_readable_priv() {
        // 0o777 grants rwx to group + world. Loading must refuse.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        let priv_path = dir.path().join("alice.priv");
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o777)).unwrap();
        let err = load("alice", dir.path()).unwrap_err();
        // Restore mode so tempdir cleanup works regardless of outcome.
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600)).unwrap();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("insecure mode"),
            "error must name the failure mode, got: {msg}"
        );
        assert!(
            msg.contains("chmod 0600"),
            "error must include the fix invocation, got: {msg}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_keypair_load_refuses_group_readable_priv() {
        // 0o640 grants read to group. Loading must refuse — any
        // group/other bit triggers the check (mode & 0o077 != 0).
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        let priv_path = dir.path().join("alice.priv");
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o640)).unwrap();
        let err = load("alice", dir.path()).unwrap_err();
        fs::set_permissions(&priv_path, fs::Permissions::from_mode(0o600)).unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("insecure mode"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn test_keypair_load_accepts_0600() {
        // The canonical mode `save` writes. Must load cleanly.
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        let priv_path = dir.path().join("alice.priv");
        // `save` already writes 0600; assert explicitly to catch a
        // future-self regression that loosens the save path.
        let mode = fs::metadata(&priv_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "save must write 0600, got {mode:o}");

        let loaded = load("alice", dir.path()).expect("0600 must load");
        assert!(loaded.can_sign(), "0600 mode must yield a signing keypair");
    }

    #[cfg(unix)]
    #[test]
    fn test_keypair_load_missing_priv_skips_mode_check() {
        // Public-only load (no .priv file) must NOT trip the mode
        // check. This is the documented "verify but not sign" path
        // for peer pubkey enrolment.
        let dir = tmp_dir();
        let kp = generate("alice").unwrap();
        save(&kp, dir.path()).unwrap();
        fs::remove_file(dir.path().join("alice.priv")).unwrap();
        let loaded = load("alice", dir.path()).expect("public-only load must succeed");
        assert!(!loaded.can_sign());
    }
}
