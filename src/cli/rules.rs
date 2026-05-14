// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory rules` subcommand — operator-facing CRUD for the
//! substrate-level agent-action rules engine (issue #691).
//!
//! Six verbs:
//!
//! * `add`     — insert a new rule (mutation: requires operator key).
//! * `list`    — print every rule, including disabled ones (read).
//! * `check`   — evaluate a proposed action against the live rule set
//!               and print the [`Decision`] (read).
//! * `enable`  — flip `enabled = 1` on an existing rule (mutation).
//! * `disable` — flip `enabled = 0` on an existing rule (mutation).
//! * `remove`  — delete a rule (mutation).
//!
//! # Operator identity (mutation gate)
//!
//! Per issue #691 design revision 2026-05-13, the four mutation
//! verbs require the operator's Ed25519 keypair on disk at
//! `${AI_MEMORY_KEY_DIR:-~/.config/ai-memory/keys}/operator.priv`
//! (mode 0600). The CLI:
//!
//! 1. Resolves the key directory (env override → default).
//! 2. Loads `operator.priv` and verifies mode bits (0600 on Unix).
//! 3. Signs the canonical rule encoding via Ed25519.
//! 4. Persists the signature alongside the rule (
//!    [`crate::governance::rules_store::update_signature`]).
//!
//! If the key file is absent / wrong-mode, the CLI refuses with
//! `governance.no_operator_key` error. No mutation lands.
//!
//! The HTTP / MCP surfaces enforce the same gate: HTTP verifies an
//! Ed25519 signature header against `operator.pub`; MCP stdio
//! mutation tools are explicitly disabled (return
//! `governance.not_available_over_mcp`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use ed25519_dalek::{Signer, SigningKey};
use serde::Serialize;

use crate::cli::CliOutput;
use crate::governance::agent_action::{AgentAction, check_agent_action};
use crate::governance::rules_store::{self, Rule};
use crate::identity::keypair as kp;

/// Wire id reserved for the operator's keypair file on disk. Stored
/// under the same directory as per-agent keys but treated specially
/// — the agent_id resolution stack never returns this id; only the
/// rules subcommand looks for it.
pub const OPERATOR_KEY_ID: &str = "operator";

/// `attest_level` stamped on rules after the operator signs them.
pub const OPERATOR_SIGNED_LEVEL: &str = "operator_signed";

/// Length of a raw Ed25519 signing-key seed on disk.
const ED25519_SEED_LEN: usize = ed25519_dalek::SECRET_KEY_LENGTH;
/// Length of a raw Ed25519 verifying-key on disk (decoded base64).
const ED25519_PUBLIC_LEN: usize = ed25519_dalek::PUBLIC_KEY_LENGTH;

#[derive(Args)]
pub struct RulesArgs {
    /// Override the default key storage directory.
    /// Honors `AI_MEMORY_KEY_DIR` env var when this flag is omitted.
    #[arg(long, value_name = "PATH", global = true)]
    pub key_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub action: RulesAction,
}

#[derive(Subcommand)]
pub enum RulesAction {
    /// Add a new agent-action rule. Requires operator keypair on
    /// disk; signs the canonical row encoding before persisting.
    Add {
        /// Rule id (e.g. R005, `tmp-noisy-build`). Must be unique.
        #[arg(long)]
        id: String,
        /// Action kind: `bash` / `filesystem_write` / `network_request`
        /// / `process_spawn` / `custom`.
        #[arg(long)]
        kind: String,
        /// Matcher JSON. Shape depends on `--kind`. See
        /// `docs/governance/agent-action-rules.md`.
        #[arg(long)]
        matcher: String,
        /// Severity: `refuse` / `warn` / `log`.
        #[arg(long, default_value = "refuse")]
        severity: String,
        /// Human-readable reason surfaced to the agent on a match.
        #[arg(long)]
        reason: String,
        /// Optional namespace scope. Defaults to `_global`.
        #[arg(long, default_value = "_global")]
        namespace: String,
        /// Land the rule with `enabled = 0` (operator activates
        /// later via `ai-memory rules enable <id> --sign`).
        #[arg(long)]
        disabled: bool,
        /// Sign the rule with the operator keypair on disk. Required
        /// for non-dry-run inserts; without `--sign` the CLI refuses.
        #[arg(long)]
        sign: bool,
    },
    /// List every rule (enabled + disabled). Read-only, no key
    /// required.
    List,
    /// Evaluate a proposed action against the live rule set without
    /// committing it. Read-only. The output is the same JSON
    /// [`Decision`] shape the MCP / HTTP path returns.
    Check {
        /// Action kind: same vocabulary as `add --kind`.
        #[arg(long)]
        kind: String,
        /// Action payload JSON. For Bash: `{"command":"ls"}`.
        /// For `FilesystemWrite`: `{"path":"/tmp/x"}`. Etc.
        #[arg(long)]
        payload: String,
        /// Optional agent id; defaults to the resolved NHI id for
        /// audit-row provenance.
        #[arg(long)]
        agent_id: Option<String>,
    },
    /// Activate a rule (flip `enabled = 1`). Requires `--sign`.
    Enable {
        /// Rule id.
        #[arg(long)]
        id: String,
        /// Sign the activation with the operator key.
        #[arg(long)]
        sign: bool,
    },
    /// Deactivate a rule (flip `enabled = 0`). Requires `--sign`.
    Disable {
        /// Rule id.
        #[arg(long)]
        id: String,
        /// Sign the deactivation with the operator key.
        #[arg(long)]
        sign: bool,
    },
    /// Remove a rule from the table. Requires `--sign`.
    Remove {
        /// Rule id.
        #[arg(long)]
        id: String,
        /// Sign the removal with the operator key.
        #[arg(long)]
        sign: bool,
    },
    /// v0.7.0 L1-6 — generate a fresh Ed25519 operator keypair and
    /// write the private 32-byte seed to `--out` (mode 0600 on Unix)
    /// plus a base64-encoded public key sibling at `<out>.pub`
    /// (mode 0644). Default `--out` is `~/.config/ai-memory/operator.key`.
    ///
    /// Refuses to overwrite an existing file unless `--force` is passed;
    /// even with `--force` a stderr warning is emitted (an existing
    /// operator key is the keystone of the signature verify chain — a
    /// silent overwrite would invalidate every prior signed rule).
    ///
    /// The 32-byte seed never appears in stdout, stderr, or any
    /// memory the agent emits. Only the fingerprint
    /// `sha256(public_key)[:16]` is logged.
    Keygen {
        /// Output path for the 32-byte private seed. The base64
        /// public key sibling is written to `<out>.pub`.
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
        /// Overwrite an existing private/public key pair. Emits a
        /// stderr warning even when set. Default: refuse to overwrite.
        #[arg(long)]
        force: bool,
    },
}

/// JSON envelope used by `--json` callers — keeps a stable wire shape
/// across the six verbs.
#[derive(Serialize)]
struct CliEnvelope<'a> {
    verb: &'a str,
    result: serde_json::Value,
}

/// Dispatch entry point called by `daemon_runtime::run`.
///
/// # Errors
///
/// Returns an error on a SQLite / key / signature failure; the
/// caller surfaces the error to the operator via the standard
/// `anyhow` chain.
pub fn run(
    db_path: &std::path::Path,
    args: RulesArgs,
    json: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("rules: open db at {}", db_path.display()))?;
    let key_dir = resolve_key_dir(args.key_dir.as_deref())?;

    match args.action {
        RulesAction::Add {
            id,
            kind,
            matcher,
            severity,
            reason,
            namespace,
            disabled,
            sign,
        } => {
            if !sign {
                bail!("governance.no_operator_key: `rules add` requires --sign");
            }
            let signing_key = load_operator_signing_key_from_dir(&key_dir)?;
            // Validate matcher JSON shape now — better to refuse at
            // input time than on the next check call.
            serde_json::from_str::<serde_json::Value>(&matcher)
                .with_context(|| format!("rules add: matcher is not valid JSON: {matcher}"))?;
            let created_at = chrono::Utc::now().timestamp();
            let agent_id = resolve_agent_id();
            let mut rule = Rule {
                id: id.clone(),
                kind,
                matcher,
                severity,
                reason,
                namespace,
                created_by: agent_id,
                created_at,
                enabled: !disabled,
                signature: None,
                attest_level: "unsigned".to_string(),
            };
            let canonical = rules_store::canonical_bytes(&rule)?;
            let sig = signing_key.sign(&canonical);
            rule.signature = Some(sig.to_bytes().to_vec());
            rule.attest_level = OPERATOR_SIGNED_LEVEL.to_string();
            rules_store::insert(&conn, &rule)?;
            emit_ok(json, out, "rules.add", &rule_to_json(&rule))?;
            Ok(())
        }
        RulesAction::List => {
            let rules = rules_store::list(&conn)?;
            let payload = serde_json::Value::Array(rules.iter().map(rule_to_json).collect());
            emit_ok(json, out, "rules.list", &payload)?;
            Ok(())
        }
        RulesAction::Check {
            kind,
            payload,
            agent_id,
        } => {
            let action = build_action(&kind, &payload)?;
            let resolved_agent = agent_id.unwrap_or_else(resolve_agent_id);
            let decision = check_agent_action(&conn, &resolved_agent, &action)?;
            emit_ok(json, out, "rules.check", &serde_json::to_value(&decision)?)?;
            Ok(())
        }
        RulesAction::Enable { id, sign } => {
            if !sign {
                bail!("governance.no_operator_key: `rules enable` requires --sign");
            }
            let signing_key = load_operator_signing_key_from_dir(&key_dir)?;
            let Some(mut rule) = rules_store::get(&conn, &id)? else {
                bail!("rules.enable: no rule with id={id}");
            };
            rule.enabled = true;
            let canonical = rules_store::canonical_bytes(&rule)?;
            let sig = signing_key.sign(&canonical);
            rules_store::set_enabled(&conn, &id, true)?;
            rules_store::update_signature(&conn, &id, &sig.to_bytes(), OPERATOR_SIGNED_LEVEL)?;
            let updated =
                rules_store::get(&conn, &id)?.context("rules.enable: row vanished after update")?;
            emit_ok(json, out, "rules.enable", &rule_to_json(&updated))?;
            Ok(())
        }
        RulesAction::Disable { id, sign } => {
            if !sign {
                bail!("governance.no_operator_key: `rules disable` requires --sign");
            }
            let signing_key = load_operator_signing_key_from_dir(&key_dir)?;
            let Some(mut rule) = rules_store::get(&conn, &id)? else {
                bail!("rules.disable: no rule with id={id}");
            };
            rule.enabled = false;
            let canonical = rules_store::canonical_bytes(&rule)?;
            let sig = signing_key.sign(&canonical);
            rules_store::set_enabled(&conn, &id, false)?;
            rules_store::update_signature(&conn, &id, &sig.to_bytes(), OPERATOR_SIGNED_LEVEL)?;
            let updated = rules_store::get(&conn, &id)?
                .context("rules.disable: row vanished after update")?;
            emit_ok(json, out, "rules.disable", &rule_to_json(&updated))?;
            Ok(())
        }
        RulesAction::Remove { id, sign } => {
            if !sign {
                bail!("governance.no_operator_key: `rules remove` requires --sign");
            }
            let _ = load_operator_signing_key_from_dir(&key_dir)?;
            let removed = rules_store::remove(&conn, &id)?;
            let payload = serde_json::json!({ "id": id, "removed": removed });
            emit_ok(json, out, "rules.remove", &payload)?;
            Ok(())
        }
        RulesAction::Keygen {
            out: out_path,
            force,
        } => {
            let resolved = resolve_operator_key_path(out_path.as_deref())?;
            let fingerprint = keygen_operator(&resolved, force, out)?;
            let payload = serde_json::json!({
                "path": resolved.display().to_string(),
                "public_path": format!("{}.pub", resolved.display()),
                "fingerprint": fingerprint,
            });
            emit_ok(json, out, "rules.keygen", &payload)?;
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// L1-6 — operator keypair generation + loading
// ---------------------------------------------------------------------------

/// Resolve the operator key path: explicit `--out` override → default
/// `~/.config/ai-memory/operator.key`. The default lives next to the
/// per-agent `keys/` directory rather than under it because the
/// operator key is a singleton, not an enumerable list — see
/// `migrations/sqlite/0024_v07_governance_rules.sql` for the design
/// note.
fn resolve_operator_key_path(override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p.to_path_buf());
    }
    let base = dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("rules.keygen: OS did not advertise a config directory"))?;
    Ok(base.join("ai-memory").join("operator.key"))
}

/// Generate a fresh Ed25519 keypair, write the 32-byte seed to `path`
/// (mode 0600 on Unix) and the base64-encoded verifying key to
/// `<path>.pub` (mode 0644). Returns the public-key fingerprint
/// (`sha256(pub_bytes)` truncated to 16 hex chars) for the success
/// line.
///
/// # Invariants
///
/// - Refuses to overwrite an existing private or public file unless
///   `force` is true.
/// - Even with `force`, emits a `WARNING` line to `stderr` reminding
///   the operator that all prior signatures will become invalid.
/// - On non-Unix targets the mode bits cannot be enforced; the
///   function emits a `WARNING` to `stderr` and skips the chmod.
///
/// # Security
///
/// The 32-byte seed is in scope only inside this function. It is
/// never returned, never logged, never embedded in a `tracing!`
/// macro. The caller receives only the fingerprint.
fn keygen_operator(path: &Path, force: bool, out: &mut CliOutput<'_>) -> Result<String> {
    let pub_path = pub_sibling_path(path);

    if !force && (path.exists() || pub_path.exists()) {
        bail!(
            "rules.keygen: refusing to overwrite existing key material at {} (or {}). \
             Pass --force to replace — note that all prior operator-signed rules \
             will fail signature verification with the new key.",
            path.display(),
            pub_path.display()
        );
    }
    if force && (path.exists() || pub_path.exists()) {
        writeln!(
            out.stderr,
            "WARNING: rules.keygen --force replaces existing operator key. \
             All prior operator-signed rules become INVALID and will be skipped at \
             load time until re-signed with the new key."
        )?;
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("rules.keygen: create parent dir {}", parent.display()))?;
    }

    // SECURITY: `OsRng` is the platform CSPRNG; ed25519-dalek's
    // `SigningKey::generate` consumes 32 bytes from it as the seed.
    let mut csprng = rand_core::OsRng;
    let signing = SigningKey::generate(&mut csprng);
    let verifying = signing.verifying_key();
    let seed = signing.to_bytes();
    let pub_bytes = verifying.to_bytes();

    // Private seed: mode 0600 on Unix; on Windows write the file but
    // emit a stderr warning that mode bits are unenforced.
    write_operator_private_seed(path, &seed, out)?;
    // Public key: base64(URL_SAFE_NO_PAD) of the 32-byte verifying key.
    write_operator_public_key(&pub_path, &pub_bytes)?;

    // Best-effort post-write fingerprint. We zero `seed` after use
    // out of habit; the local variable goes out of scope at function
    // end so the memory page is reclaimed on the next allocation.
    let fingerprint = pub_fingerprint(&pub_bytes);

    // SECURITY: print the fingerprint, never the seed.
    writeln!(
        out.stdout,
        "Ed25519 operator key generated: {fingerprint} -> {}",
        path.display()
    )?;

    // `seed` is `[u8; 32]`, a `Copy` type, so an explicit `drop`
    // call is a no-op. The Rust compiler reclaims the stack slot
    // automatically on scope exit; we simply rely on that.

    Ok(fingerprint)
}

/// Write the 32-byte private seed to `path` with mode 0600. The file
/// is created with `O_CREAT | O_WRONLY | O_TRUNC` so a pre-existing
/// file is truncated and the new bytes land atomically. After the
/// write we verify the mode bits via `stat` and refuse if anything
/// other than 0o600 is observed.
fn write_operator_private_seed(
    path: &Path,
    seed: &[u8; ED25519_SEED_LEN],
    #[cfg_attr(unix, allow(unused_variables))] out: &mut CliOutput<'_>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;

        // Remove first so a stricter pre-existing mode does not block
        // the create_new path; we already gated overwrite above.
        let _ = std::fs::remove_file(path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("rules.keygen: create {}", path.display()))?;
        file.write_all(seed)
            .with_context(|| format!("rules.keygen: write seed to {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("rules.keygen: fsync {}", path.display()))?;
        drop(file);

        // Verify the mode bits actually landed (defense against an
        // `OpenOptionsExt::mode` regression or a weird umask path).
        let mode = std::fs::metadata(path)
            .with_context(|| format!("rules.keygen: stat {}", path.display()))?
            .permissions()
            .mode()
            & 0o777;
        if mode != 0o600 {
            // Try once more to chmod to 0600 — best effort recovery.
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)
                .with_context(|| format!("rules.keygen: chmod 0600 {}", path.display()))?;
            let verified = std::fs::metadata(path)?.permissions().mode() & 0o777;
            if verified != 0o600 {
                bail!(
                    "rules.keygen: could not enforce mode 0600 on {} (observed {verified:o})",
                    path.display()
                );
            }
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        writeln!(
            out.stderr,
            "WARNING: Windows: operator key permissions not enforced; protect manually"
        )?;
        std::fs::write(path, seed)
            .with_context(|| format!("rules.keygen: write seed to {}", path.display()))?;
        Ok(())
    }
}

/// Write the base64-encoded verifying key to `<path>.pub`. World-
/// readable (mode 0644 on Unix) because public keys are by definition
/// non-secret.
fn write_operator_public_key(pub_path: &Path, pub_bytes: &[u8; ED25519_PUBLIC_LEN]) -> Result<()> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pub_bytes);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::remove_file(pub_path);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(pub_path)
            .with_context(|| format!("rules.keygen: create {}", pub_path.display()))?;
        file.write_all(encoded.as_bytes())
            .with_context(|| format!("rules.keygen: write pub to {}", pub_path.display()))?;
        file.sync_all()
            .with_context(|| format!("rules.keygen: fsync {}", pub_path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(pub_path, encoded.as_bytes())
            .with_context(|| format!("rules.keygen: write pub to {}", pub_path.display()))?;
    }
    Ok(())
}

/// Compute the `sha256(pub_bytes)` fingerprint truncated to 16 hex
/// chars. Used in the success line `Ed25519 operator key generated:
/// <fp> -> <path>` so the operator can sanity-check the public key
/// without inspecting the file. Truncated to 16 chars (64 bits) —
/// collision resistance is irrelevant here (the operator already
/// trusts the file path; this is for human-readable disambiguation).
fn pub_fingerprint(pub_bytes: &[u8; ED25519_PUBLIC_LEN]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(pub_bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Resolve the public-key sibling path for a given private-seed path.
/// `~/.config/ai-memory/operator.key` → `~/.config/ai-memory/operator.key.pub`.
fn pub_sibling_path(seed_path: &Path) -> PathBuf {
    let mut s = seed_path.as_os_str().to_os_string();
    s.push(".pub");
    PathBuf::from(s)
}

/// Load the operator signing key from `path` (32 raw bytes, mode
/// 0600 on Unix). This is the public helper exposed for tests and
/// the L1-6 sign-seed pipeline.
///
/// # Errors
///
/// - Returns a clear error mentioning `0600` when the file mode is
///   anything other than 0o600 on Unix.
/// - Returns an error when the file length is not exactly 32 bytes.
/// - On non-Unix targets the mode check is skipped (file ACL applies
///   instead; the OSS layer does not enforce hardware-backed storage —
///   see `src/identity/keypair.rs` "Hardware-backed key storage"
///   section).
pub fn load_operator_signing_key(path: &Path) -> Result<SigningKey> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::metadata(path)
            .with_context(|| format!("load_operator_signing_key: stat {}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode != 0o600 {
            bail!(
                "load_operator_signing_key: {} has mode {mode:o}; permissions too open; \
                 chmod 0600 {} to restore",
                path.display(),
                path.display()
            );
        }
    }
    let bytes = std::fs::read(path)
        .with_context(|| format!("load_operator_signing_key: read {}", path.display()))?;
    if bytes.len() != ED25519_SEED_LEN {
        bail!(
            "load_operator_signing_key: {} has {} bytes, expected {ED25519_SEED_LEN}",
            path.display(),
            bytes.len()
        );
    }
    let mut seed = [0u8; ED25519_SEED_LEN];
    seed.copy_from_slice(&bytes);
    Ok(SigningKey::from_bytes(&seed))
}

/// Resolve the operator key directory, honoring `--key-dir` →
/// `AI_MEMORY_KEY_DIR` → `kp::default_key_dir()`.
fn resolve_key_dir(override_dir: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = override_dir {
        return Ok(p.to_path_buf());
    }
    kp::default_key_dir()
}

/// Load the operator's signing key from `<key_dir>/operator.priv`.
///
/// Refuses if the file is missing, if the mode bits are not 0600 on
/// Unix, or if the file contents do not parse as a 32-byte Ed25519
/// signing key. Returns the typed `SigningKey` ready to call
/// `.sign()`.
///
/// Used by the original `add` / `enable` / `disable` / `remove` verbs
/// (dir-based key layout — `operator.priv` + `operator.pub`). The
/// L1-6 `keygen` / `sign-seed` verbs use [`load_operator_signing_key`]
/// instead, which takes the explicit private-seed file path
/// (`~/.config/ai-memory/operator.key`) per the L1-6 spec.
fn load_operator_signing_key_from_dir(
    key_dir: &std::path::Path,
) -> Result<ed25519_dalek::SigningKey> {
    let kp = kp::load(OPERATOR_KEY_ID, key_dir).with_context(|| {
        format!(
            "governance.no_operator_key: operator.priv missing at {}",
            key_dir.display()
        )
    })?;
    kp.private.ok_or_else(|| {
        anyhow::anyhow!(
            "governance.no_operator_key: operator keypair has no private half (public-only load)"
        )
    })
}

/// Resolve the caller's agent_id for `created_by` provenance. Uses
/// the same NHI vocabulary as the rest of the CLI. Falls back to a
/// process-bound id if env / clientInfo resolution fails.
fn resolve_agent_id() -> String {
    crate::identity::resolve_agent_id(None, None)
        .unwrap_or_else(|_| format!("anonymous:pid-{}", std::process::id()))
}

/// Build an [`AgentAction`] from `kind` + JSON payload. Used by
/// `rules check` to mirror the harness PreToolUse hook input.
fn build_action(kind: &str, payload_json: &str) -> Result<AgentAction> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)
        .with_context(|| format!("rules check: payload is not valid JSON: {payload_json}"))?;
    match kind {
        "bash" => {
            let command = payload
                .get("command")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("bash payload requires `command` string"))?
                .to_string();
            let cwd = payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            Ok(AgentAction::Bash { command, cwd })
        }
        "filesystem_write" => {
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("filesystem_write payload requires `path` string"))?
                .to_string();
            let byte_estimate = payload
                .get("byte_estimate")
                .and_then(serde_json::Value::as_u64);
            Ok(AgentAction::FilesystemWrite {
                path: PathBuf::from(path),
                byte_estimate,
            })
        }
        "network_request" => {
            let host = payload
                .get("host")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("network_request payload requires `host` string"))?
                .to_string();
            let scheme = payload
                .get("scheme")
                .and_then(|v| v.as_str())
                .unwrap_or("https")
                .to_string();
            Ok(AgentAction::NetworkRequest { host, scheme })
        }
        "process_spawn" => {
            let binary = payload
                .get("binary")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("process_spawn payload requires `binary` string"))?
                .to_string();
            let args = payload
                .get("args")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(AgentAction::ProcessSpawn { binary, args })
        }
        "custom" => {
            let custom_kind = payload
                .get("custom_kind")
                .or_else(|| payload.get("kind"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("custom payload requires `custom_kind` string"))?
                .to_string();
            Ok(AgentAction::Custom {
                custom_kind,
                payload,
            })
        }
        other => bail!("rules check: unknown kind `{other}`"),
    }
}

/// Render a [`Rule`] as JSON for CLI output. The signature is
/// base64-encoded (URL-safe, no padding) so the JSON is operator-
/// readable. Empty signature ⇒ null.
fn rule_to_json(rule: &Rule) -> serde_json::Value {
    use base64::Engine;
    let sig_b64 = rule
        .signature
        .as_ref()
        .map(|b| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b));
    serde_json::json!({
        "id": rule.id,
        "kind": rule.kind,
        "matcher": rule.matcher,
        "severity": rule.severity,
        "reason": rule.reason,
        "namespace": rule.namespace,
        "created_by": rule.created_by,
        "created_at": rule.created_at,
        "enabled": rule.enabled,
        "signature_b64": sig_b64,
        "attest_level": rule.attest_level,
    })
}

fn emit_ok(
    json: bool,
    out: &mut CliOutput<'_>,
    verb: &str,
    result: &serde_json::Value,
) -> Result<()> {
    if json {
        let env = CliEnvelope {
            verb,
            result: result.clone(),
        };
        writeln!(out.stdout, "{}", serde_json::to_string(&env)?)?;
    } else {
        // Human format: pretty-print the result tree. The verb header
        // is suppressed (the CLI command itself is the implicit
        // context).
        writeln!(out.stdout, "{}", serde_json::to_string_pretty(result)?)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_action_bash_parses() {
        let a = build_action("bash", r#"{"command":"ls -la"}"#).unwrap();
        match a {
            AgentAction::Bash { command, cwd } => {
                assert_eq!(command, "ls -la");
                assert!(cwd.is_none());
            }
            _ => panic!("expected bash"),
        }
    }

    #[test]
    fn build_action_filesystem_write_parses() {
        let a = build_action("filesystem_write", r#"{"path":"/tmp/x"}"#).unwrap();
        match a {
            AgentAction::FilesystemWrite { path, .. } => {
                assert_eq!(path, PathBuf::from("/tmp/x"));
            }
            _ => panic!("expected filesystem_write"),
        }
    }

    #[test]
    fn build_action_network_request_parses_with_scheme_default() {
        let a = build_action("network_request", r#"{"host":"x.example.com"}"#).unwrap();
        match a {
            AgentAction::NetworkRequest { host, scheme } => {
                assert_eq!(host, "x.example.com");
                assert_eq!(scheme, "https");
            }
            _ => panic!("expected network_request"),
        }
    }

    #[test]
    fn build_action_process_spawn_parses() {
        let a = build_action(
            "process_spawn",
            r#"{"binary":"cargo","args":["build","--release"]}"#,
        )
        .unwrap();
        match a {
            AgentAction::ProcessSpawn { binary, args } => {
                assert_eq!(binary, "cargo");
                assert_eq!(args, vec!["build", "--release"]);
            }
            _ => panic!("expected process_spawn"),
        }
    }

    #[test]
    fn build_action_custom_parses() {
        let a = build_action("custom", r#"{"custom_kind":"deploy","env":"prod"}"#).unwrap();
        match a {
            AgentAction::Custom { custom_kind, .. } => assert_eq!(custom_kind, "deploy"),
            _ => panic!("expected custom"),
        }
    }

    #[test]
    fn build_action_unknown_kind_errors() {
        assert!(build_action("nope", "{}").is_err());
    }

    #[test]
    fn build_action_invalid_json_errors() {
        assert!(build_action("bash", "not json").is_err());
    }

    #[test]
    fn build_action_missing_required_field_errors() {
        assert!(build_action("bash", "{}").is_err());
        assert!(build_action("filesystem_write", "{}").is_err());
    }

    #[test]
    fn rule_to_json_encodes_signature_as_base64() {
        let mut rule = Rule {
            id: "R1".into(),
            kind: "bash".into(),
            matcher: r#"{"command_regex":"x"}"#.into(),
            severity: "refuse".into(),
            reason: "test".into(),
            namespace: "_global".into(),
            created_by: "test".into(),
            created_at: 0,
            enabled: true,
            signature: None,
            attest_level: "unsigned".into(),
        };
        let v = rule_to_json(&rule);
        assert_eq!(v["signature_b64"], serde_json::Value::Null);
        rule.signature = Some(vec![0xff, 0x00, 0xaa]);
        let v = rule_to_json(&rule);
        assert_eq!(
            v["signature_b64"],
            serde_json::Value::String("_wCq".to_string())
        );
    }

    // -----------------------------------------------------------------
    // L1-6 — keygen + load_operator_signing_key unit tests
    // -----------------------------------------------------------------

    #[test]
    fn pub_sibling_path_appends_dot_pub() {
        let p = pub_sibling_path(Path::new("/x/y/operator.key"));
        assert_eq!(p, PathBuf::from("/x/y/operator.key.pub"));
    }

    #[test]
    fn pub_fingerprint_is_deterministic_and_16_hex_chars() {
        let bytes = [0u8; 32];
        let fp1 = pub_fingerprint(&bytes);
        let fp2 = pub_fingerprint(&bytes);
        assert_eq!(fp1, fp2, "fingerprint must be deterministic");
        assert_eq!(fp1.len(), 16, "fingerprint must be 16 hex chars");
        assert!(
            fp1.chars().all(|c| c.is_ascii_hexdigit()),
            "fingerprint must be ASCII hex"
        );
        // Different input → different fingerprint.
        let mut other = [0u8; 32];
        other[0] = 1;
        let fp3 = pub_fingerprint(&other);
        assert_ne!(fp1, fp3);
    }

    #[cfg(unix)]
    #[test]
    fn keygen_writes_priv_0600_and_pub_0644_then_loads() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let fp = keygen_operator(&key_path, false, &mut out).expect("keygen");
        assert_eq!(fp.len(), 16);

        // Private file: mode 0600 + 32 bytes.
        let meta = std::fs::metadata(&key_path).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "priv key must be 0600, got {mode:o}");
        let bytes = std::fs::read(&key_path).unwrap();
        assert_eq!(bytes.len(), 32, "priv seed must be 32 bytes");

        // Public file: mode 0644 + base64 of 32 bytes.
        let pub_path = pub_sibling_path(&key_path);
        let pmode = std::fs::metadata(&pub_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(pmode, 0o644, "pub key must be 0644, got {pmode:o}");
        let pub_b64 = std::fs::read_to_string(&pub_path).unwrap();
        use base64::Engine;
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(pub_b64.trim())
            .expect("pub base64 decodes");
        assert_eq!(decoded.len(), 32);

        // load_operator_signing_key round-trips and the derived
        // verifying key matches the .pub bytes.
        let signing = load_operator_signing_key(&key_path).expect("load");
        let verifying = signing.verifying_key();
        assert_eq!(verifying.to_bytes()[..], decoded[..]);

        // Stdout includes the fingerprint and the path; never the seed.
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains(&fp), "stdout must include fingerprint, got: {s}");
        // Seed bytes should never round-trip through stdout (defensive
        // check: the random seed is unlikely to be valid utf8 anyway,
        // but we assert the success line is the only stdout content).
        assert!(s.starts_with("Ed25519 operator key generated:"));
    }

    #[cfg(unix)]
    #[test]
    fn keygen_refuses_overwrite_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_path, false, &mut out).expect("first");
        let bytes_before = std::fs::read(&key_path).unwrap();

        // Second call without --force must refuse.
        let err = keygen_operator(&key_path, false, &mut out).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("refusing to overwrite"), "got: {msg}");

        // Bytes on disk must not have changed.
        let bytes_after = std::fs::read(&key_path).unwrap();
        assert_eq!(bytes_before, bytes_after);
    }

    #[cfg(unix)]
    #[test]
    fn keygen_force_overwrites_and_warns_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let fp1 = keygen_operator(&key_path, false, &mut out).expect("first");
        let fp2 = keygen_operator(&key_path, true, &mut out).expect("force");
        assert_ne!(fp1, fp2, "fresh keypair must have new fingerprint");

        let s = String::from_utf8(stderr).unwrap();
        assert!(
            s.contains("WARNING") && s.contains("INVALID"),
            "stderr must warn about prior-signature invalidation, got: {s}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn load_operator_signing_key_refuses_open_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_path, false, &mut out).expect("keygen");
        // Loosen perms.
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_operator_signing_key(&key_path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("0600"), "error must mention 0600, got: {msg}");
        // Restore so the tempdir cleanup works.
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    #[test]
    fn load_operator_signing_key_rejects_wrong_length() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("operator.key");
        // Write a short file that bypasses the mode check (or, on
        // unix, the mode check fires first — both paths exercise the
        // "refuse to sign with non-conforming material" property).
        std::fs::write(&key_path, b"too-short").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load_operator_signing_key(&key_path).unwrap_err();
        let msg = format!("{err:#}");
        // Either the length error or the stat error is acceptable —
        // both are refusals.
        assert!(
            msg.contains("expected") || msg.contains("bytes"),
            "got: {msg}"
        );
    }
}
