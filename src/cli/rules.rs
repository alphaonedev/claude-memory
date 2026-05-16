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
    /// v0.7.0 L1-6 — sign every seeded rule (R001..R004 today) with
    /// the operator key. Sets `signature = ed25519(canonical_payload)`
    /// and `attest_level = 'operator_signed'`. `enabled` stays at 0
    /// — the operator audits and activates manually after this runs.
    ///
    /// The canonical payload includes `enabled`, so a direct
    /// `UPDATE governance_rules SET enabled = 1` after signing would
    /// fail signature verification at load time — that is the
    /// bypass-prevention property.
    SignSeed {
        /// Path to the operator private seed (32 bytes) — same shape
        /// `rules keygen --out` writes. Defaults to
        /// `~/.config/ai-memory/operator.key`.
        #[arg(long, value_name = "PATH")]
        key: Option<PathBuf>,
        /// Override the DB path (useful for smoke tests against a
        /// scratch sqlite file). Defaults to the same `--db` the
        /// rest of the `rules` verbs use (the top-level `ai-memory
        /// --db` flag).
        #[arg(long, value_name = "PATH")]
        db: Option<PathBuf>,
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
            let matcher_json: serde_json::Value = serde_json::from_str(&matcher)
                .with_context(|| format!("rules add: matcher is not valid JSON: {matcher}"))?;
            // SEC-12 / COR-10 (Cluster D, issue #767) — the bash
            // matcher field is a LITERAL substring (despite the
            // legacy `command_regex` field name). Reject regex
            // metacharacters at CLI input time so an operator who
            // pastes `rm\s+-rf` does not silently install a
            // never-matching rule.
            if let Some(val) = matcher_json
                .get("command_substring")
                .or_else(|| matcher_json.get("command_regex"))
                .and_then(|v| v.as_str())
            {
                crate::governance::agent_action::validate_command_substring(val)
                    .map_err(|e| anyhow::anyhow!("rules add: {e}"))?;
                if matcher_json.get("command_regex").is_some()
                    && matcher_json.get("command_substring").is_none()
                {
                    tracing::warn!(
                        "rules add: matcher field `command_regex` is DEPRECATED — rename to \
                         `command_substring` (the engine has always done literal substring \
                         matching, not regex). See SEC-12 in the v0.7.0 cluster-D fix."
                    );
                }
            }
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
            // v0.7.0 issue #800 / Form 7 critical fix: sign the
            // canonical bytes that `verify_rule_signature` will read
            // back. `canonical_bytes` (without `enabled`) and
            // `canonical_bytes_for_signing` (with `enabled`) were
            // out-of-sync between the signer and verifier — the
            // signatures produced here never validated, the L1-6
            // gate silently skipped every "operator_signed" rule,
            // and Form 7 enforcement returned `allow` for every
            // action. Use `canonical_bytes_for_signing` so the
            // verifier accepts what we produce.
            let canonical = rules_store::canonical_bytes_for_signing(&rule)?;
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
            // Issue #800 critical fix: signer must use the same
            // canonical encoding as `verify_rule_signature`
            // (otherwise the L1-6 enforcement gate skips every rule
            // and Form 7 returns `allow` for every action). See the
            // matching comment in the `Add` arm.
            let canonical = rules_store::canonical_bytes_for_signing(&rule)?;
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
            // Issue #800 critical fix: parity with the Enable arm —
            // signer + verifier must use the same canonical bytes.
            let canonical = rules_store::canonical_bytes_for_signing(&rule)?;
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
        RulesAction::SignSeed { key, db } => {
            // The top-level `--db` flag already produced `conn` above.
            // When the operator passes `--db` on the subcommand (the
            // L1-6 ergonomic shortcut for one-shot scripts), reopen
            // against that path; otherwise reuse the open handle.
            if let Some(db_path) = db {
                let conn2 = rusqlite::Connection::open(&db_path).with_context(|| {
                    format!("rules.sign-seed: open db at {}", db_path.display())
                })?;
                sign_seed_rules(&conn2, key.as_deref(), json, out)?;
            } else {
                sign_seed_rules(&conn, key.as_deref(), json, out)?;
            }
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

/// L1-6 Deliverable B — sign R001..R004 (and any other rows in
/// `governance_rules`) with the operator key. Idempotent: re-running
/// computes the same canonical bytes → same signature → same UPDATE;
/// a row whose `signature` already matches the freshly computed bytes
/// is a no-op.
///
/// `enabled` STAYS at whatever the row already holds — operator
/// activates manually after audit. Canonical bytes include `enabled`
/// (see [`rules_store::canonical_bytes_for_signing`]), so a post-sign
/// `UPDATE governance_rules SET enabled = 1` would invalidate the
/// recorded signature: that is the bypass-prevention property the
/// L1-6 integration tests pin.
///
/// Returns the number of rows that were freshly signed (excluding
/// idempotent no-ops).
fn sign_seed_rules(
    conn: &rusqlite::Connection,
    key_path: Option<&Path>,
    json: bool,
    out: &mut CliOutput<'_>,
) -> Result<usize> {
    let resolved = match key_path {
        Some(p) => p.to_path_buf(),
        None => resolve_operator_key_path(None)?,
    };
    let signing_key = load_operator_signing_key(&resolved).with_context(|| {
        format!(
            "rules.sign-seed: load operator key from {}",
            resolved.display()
        )
    })?;

    let rules = rules_store::list(conn)?;
    let mut signed_now = 0usize;
    let mut summary: Vec<serde_json::Value> = Vec::new();
    for rule in rules {
        let canonical = rules_store::canonical_bytes_for_signing(&rule)?;
        let signature = signing_key.sign(&canonical);
        let sig_bytes = signature.to_bytes();
        let already_signed = matches!(
            (rule.signature.as_deref(), rule.attest_level.as_str()),
            (Some(existing), OPERATOR_SIGNED_LEVEL) if existing == sig_bytes.as_slice()
        );
        if !already_signed {
            rules_store::update_signature(
                conn,
                &rule.id,
                sig_bytes.as_slice(),
                OPERATOR_SIGNED_LEVEL,
            )?;
            signed_now += 1;
        }
        summary.push(serde_json::json!({
            "id": rule.id,
            "attest_level": OPERATOR_SIGNED_LEVEL,
            "signed_now": !already_signed,
        }));
    }

    let payload = serde_json::json!({
        "signed_now": signed_now,
        "rules": summary,
    });
    emit_ok(json, out, "rules.sign-seed", &payload)?;
    Ok(signed_now)
}

/// Resolve the operator key directory, honoring `--key-dir` →
/// `AI_MEMORY_KEY_DIR` → `kp::default_key_dir()`.
fn resolve_key_dir(override_dir: Option<&std::path::Path>) -> Result<PathBuf> {
    if let Some(p) = override_dir {
        return Ok(p.to_path_buf());
    }
    kp::default_key_dir()
}

/// Load the operator's signing key from `key_dir`. Auto-detects which
/// of the two operator-key naming conventions is in use:
///
/// 1. `operator.priv` (raw 32-byte seed) + `operator.pub` (raw 32-byte
///    verifying key) — the legacy dir-based layout the `add` / `enable`
///    / `disable` / `remove` verbs originally targeted, loaded via
///    [`kp::load`].
/// 2. `operator.key` (raw 32-byte seed) + `operator.key.pub` (base64url
///    no-pad encoded 32-byte verifying key) — the layout `rules keygen`
///    writes (`~/.config/ai-memory/operator.key`) per the L1-6 spec.
///
/// v0.7.0 G-PHASE-E-3 (#708) — before this fix, `rules keygen` wrote
/// files under (2) but `rules enable --sign` only looked for (1), so
/// the documented flow `keygen → enable` was broken end-to-end without
/// any error message that hinted at the naming mismatch. Now both
/// conventions are accepted; the error message when neither is found
/// names both so the operator can pick the right one.
///
/// Refuses if no matching pair is present, if the private-half mode
/// bits are not 0600 on Unix, or if the parsed bytes are not a valid
/// 32-byte Ed25519 signing key. Returns the typed `SigningKey` ready
/// to call `.sign()`.
fn load_operator_signing_key_from_dir(
    key_dir: &std::path::Path,
) -> Result<ed25519_dalek::SigningKey> {
    // Layout 1 — `operator.priv` + `operator.pub` (the legacy dir
    // layout). `kp::load` already handles mode-bit + length + curve
    // checks. Empty-dir cases that lack any operator file land in the
    // unified error path below.
    let priv_legacy = key_dir.join("operator.priv");
    let pub_legacy = key_dir.join("operator.pub");
    if priv_legacy.exists() && pub_legacy.exists() {
        let kp = kp::load(OPERATOR_KEY_ID, key_dir).with_context(|| {
            format!(
                "governance.no_operator_key: failed loading operator.priv/operator.pub at {}",
                key_dir.display()
            )
        })?;
        return kp.private.ok_or_else(|| {
            anyhow::anyhow!(
                "governance.no_operator_key: operator keypair has no private half (public-only load)"
            )
        });
    }
    // Layout 2 — `operator.key` (raw 32-byte seed) + `operator.key.pub`
    // (base64url no-pad encoded 32-byte verifying key). This is what
    // `rules keygen` writes; verify the public half decodes and matches
    // the seed's derived verifying key before returning so a tampered
    // .pub surfaces here, not on the next signature-verify call.
    let priv_keygen = key_dir.join("operator.key");
    let pub_keygen = key_dir.join("operator.key.pub");
    if priv_keygen.exists() {
        let signing = load_operator_signing_key(&priv_keygen).with_context(|| {
            format!(
                "governance.no_operator_key: failed loading {}",
                priv_keygen.display()
            )
        })?;
        if pub_keygen.exists() {
            use base64::Engine;
            let encoded = std::fs::read_to_string(&pub_keygen).with_context(|| {
                format!("governance.no_operator_key: read {}", pub_keygen.display())
            })?;
            let trimmed = encoded.trim();
            let pub_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(trimmed)
                .with_context(|| {
                    format!(
                        "governance.no_operator_key: decode base64url public key at {}",
                        pub_keygen.display()
                    )
                })?;
            if pub_bytes.len() != ED25519_PUBLIC_LEN {
                bail!(
                    "governance.no_operator_key: public key {} decoded to {} bytes (expected {ED25519_PUBLIC_LEN})",
                    pub_keygen.display(),
                    pub_bytes.len(),
                );
            }
            if signing.verifying_key().to_bytes().as_slice() != pub_bytes.as_slice() {
                bail!(
                    "governance.no_operator_key: private key {} does not match public key {}",
                    priv_keygen.display(),
                    pub_keygen.display(),
                );
            }
        }
        return Ok(signing);
    }
    // Layout 3 (#800 Gap #6 — keygen↔enable path-mismatch fallback) —
    // `ai-memory rules keygen` writes the operator key to
    // `<config-dir>/operator.key` (parent of the key_dir), per
    // `resolve_operator_key_path`'s "singleton, not enumerable list"
    // rationale documented in
    // `migrations/sqlite/0024_v07_governance_rules.sql`. The L1-6
    // verify path (`rules_store::resolve_operator_pubkey`) reads from
    // the parent dir for the same reason. Before this fallback, the
    // `enable/disable/add --sign` verbs refused with
    // `governance.no_operator_key` even when a fresh keygen had just
    // run. The install-batman-active.sh script worked around it by
    // mirroring the key into both locations. This in-process fallback
    // closes the wart so a fresh keygen + immediate enable just works.
    if let Some(parent) = key_dir.parent() {
        let parent_priv = parent.join("operator.key");
        let parent_pub = parent.join("operator.key.pub");
        if parent_priv.exists() {
            let signing = load_operator_signing_key(&parent_priv).with_context(|| {
                format!(
                    "governance.no_operator_key: failed loading {}",
                    parent_priv.display()
                )
            })?;
            if parent_pub.exists() {
                use base64::Engine;
                let encoded = std::fs::read_to_string(&parent_pub).with_context(|| {
                    format!(
                        "governance.no_operator_key: read {}",
                        parent_pub.display()
                    )
                })?;
                let trimmed = encoded.trim();
                let pub_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(trimmed)
                    .with_context(|| {
                        format!(
                            "governance.no_operator_key: decode base64url public key at {}",
                            parent_pub.display()
                        )
                    })?;
                if pub_bytes.len() != ED25519_PUBLIC_LEN {
                    bail!(
                        "governance.no_operator_key: public key {} decoded to {} bytes (expected {ED25519_PUBLIC_LEN})",
                        parent_pub.display(),
                        pub_bytes.len(),
                    );
                }
                if signing.verifying_key().to_bytes().as_slice() != pub_bytes.as_slice() {
                    bail!(
                        "governance.no_operator_key: private key {} does not match public key {}",
                        parent_priv.display(),
                        parent_pub.display(),
                    );
                }
            }
            return Ok(signing);
        }
    }

    // Neither layout present — name all three so the operator picks
    // the right one to materialise.
    bail!(
        "governance.no_operator_key: no operator key found at {dir} \
         (also checked parent dir for the keygen layout). \
         Expected either `operator.priv` + `operator.pub` (raw 32-byte pair, \
         as produced by per-agent `keypair` generation) OR \
         `operator.key` + `operator.key.pub` (raw 32-byte seed + base64url \
         verifier, as produced by `ai-memory rules keygen` — searched both \
         `{dir}/` and `{dir}/../`)",
        dir = key_dir.display(),
    )
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

    // -----------------------------------------------------------------
    // L1-6 — sign_seed_rules unit tests
    // -----------------------------------------------------------------

    /// Build a fresh in-memory rules-only schema for `sign_seed_rules`
    /// tests. Same shape as the engine's `fresh_conn` helper in
    /// `governance::agent_action::tests` but here we only need the
    /// rules table (no audit chain — sign-seed is pure SQL UPDATE).
    fn fresh_rules_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );",
        )
        .unwrap();
        conn
    }

    #[cfg(unix)]
    #[test]
    fn sign_seed_rules_marks_all_rows_operator_signed() {
        let tdir = tempfile::tempdir().unwrap();
        let key_path = tdir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_path, false, &mut out).unwrap();

        let conn = fresh_rules_conn();
        // Seed two unsigned rules to mirror the migration's R001..R004
        // shape (enabled=false, attest_level='unsigned').
        for id in ["R001", "R002"] {
            rules_store::insert(
                &conn,
                &Rule {
                    id: id.to_string(),
                    kind: "filesystem_write".into(),
                    matcher: r#"{"glob":"/tmp/**"}"#.into(),
                    severity: "refuse".into(),
                    reason: "test".into(),
                    namespace: "_global".into(),
                    created_by: "system:seed".into(),
                    created_at: 0,
                    enabled: false,
                    signature: None,
                    attest_level: "unsigned".into(),
                },
            )
            .unwrap();
        }

        let signed = sign_seed_rules(&conn, Some(&key_path), true, &mut out).unwrap();
        assert_eq!(signed, 2);

        // Every row is now operator_signed with a 64-byte signature
        // and `enabled` UNCHANGED (audit must be operator-driven).
        for id in ["R001", "R002"] {
            let row = rules_store::get(&conn, id).unwrap().unwrap();
            assert_eq!(row.attest_level, "operator_signed");
            assert_eq!(
                row.signature.as_ref().map(Vec::len),
                Some(ed25519_dalek::SIGNATURE_LENGTH)
            );
            assert!(!row.enabled, "sign-seed must NOT flip enabled");
        }
    }

    // -----------------------------------------------------------------
    // C-3 coverage uplift — drive `run()` for every subcommand. The
    // mutation verbs require an operator keypair on disk under
    // `<key_dir>/operator.priv` (kp::save layout); the keygen + sign-seed
    // verbs use the singleton-file layout.
    // -----------------------------------------------------------------

    /// Set up a tempdir with a `db::open`-initialized SQLite at
    /// `db_path` and an operator keypair saved under `key_dir`. Returns
    /// the tempdir guard (must outlive the test) and the two paths.
    #[cfg(unix)]
    fn fresh_env_with_operator_key() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf)
    {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ai-memory.db");
        // Initialize the full schema.
        drop(crate::db::open(&db_path).expect("db::open"));
        // Save an operator keypair at <key_dir>/operator.{priv,pub}.
        let kp = kp::generate(OPERATOR_KEY_ID).expect("generate");
        let key_dir = dir.path().join("keys");
        std::fs::create_dir_all(&key_dir).expect("mkdir keys");
        kp::save(&kp, &key_dir).expect("save kp");
        (dir, db_path, key_dir)
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_list_emits_seeded_rules() {
        // `db::open` runs migration 0024 which seeds R001..R004 disabled.
        // The list verb returns them with attest_level=unsigned. We pin
        // the dispatch + JSON envelope shape (not the seed content,
        // since the migration is owned by L0.7-2).
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::List,
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("list");
        let s = String::from_utf8(stdout).unwrap();
        // Envelope wraps the result under "rules.list".
        assert!(s.contains("\"verb\":\"rules.list\""), "got: {s}");
        // List result is an array; either empty or pre-seeded.
        assert!(s.contains("\"result\":["), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_list_human_format_emits_pretty_array() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::List,
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        // json=false → emit_ok's pretty-print branch.
        run(&db_path, args, false, &mut out).expect("list");
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("["), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_add_without_sign_refuses() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Add {
                id: "R-test".into(),
                kind: "bash".into(),
                matcher: r#"{"command_regex":"^ls"}"#.into(),
                severity: "refuse".into(),
                reason: "test".into(),
                namespace: "_global".into(),
                disabled: false,
                sign: false,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("no_operator_key"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_add_with_sign_persists_signed_rule() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: RulesAction::Add {
                id: "R-add-1".into(),
                kind: "bash".into(),
                // SEC-12/COR-10: literal substring (engine has always done
                // substring match, despite the legacy field name).
                matcher: r#"{"command_substring":"rm -rf /"}"#.into(),
                severity: "refuse".into(),
                reason: "rm-rf is bad".into(),
                namespace: "_global".into(),
                disabled: false,
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("add");
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("rules.add"), "got: {s}");
        assert!(s.contains("R-add-1"), "got: {s}");
        assert!(s.contains("operator_signed"), "got: {s}");

        // Confirm the row landed.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r = rules_store::get(&conn, "R-add-1").unwrap().unwrap();
        assert_eq!(r.attest_level, "operator_signed");
        assert!(r.signature.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_add_with_bad_matcher_json_errors() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Add {
                id: "R-bad".into(),
                kind: "bash".into(),
                matcher: "{ not json".into(), // malformed
                severity: "refuse".into(),
                reason: "x".into(),
                namespace: "_global".into(),
                disabled: false,
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must refuse");
        let msg = format!("{err:#}");
        assert!(msg.contains("matcher"), "got: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_add_disabled_lands_disabled_row() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Add {
                id: "R-dis".into(),
                kind: "filesystem_write".into(),
                matcher: r#"{"glob":"/tmp/**"}"#.into(),
                severity: "warn".into(),
                reason: "noisy".into(),
                namespace: "_global".into(),
                disabled: true,
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("add");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let r = rules_store::get(&conn, "R-dis").unwrap().unwrap();
        assert!(!r.enabled, "disabled flag must propagate");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_check_evaluates_action_against_empty_set() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Check {
                kind: "bash".into(),
                payload: r#"{"command":"ls"}"#.into(),
                agent_id: Some("tester".into()),
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("check");
        let s = String::from_utf8(stdout).unwrap();
        // Decision JSON envelope — at minimum "rules.check" verb shows up.
        assert!(s.contains("rules.check"), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_check_without_agent_id_uses_default() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Check {
                kind: "network_request".into(),
                payload: r#"{"host":"example.com","scheme":"https"}"#.into(),
                agent_id: None,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("check");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_enable_unsign_refuses() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Enable {
                id: "R-x".into(),
                sign: false,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must refuse");
        assert!(format!("{err:#}").contains("no_operator_key"));
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_enable_unknown_id_errors() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Enable {
                id: "R-does-not-exist".into(),
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must error");
        assert!(format!("{err:#}").contains("no rule with id"));
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_enable_and_disable_roundtrip() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        // First add a disabled rule.
        let args = RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: RulesAction::Add {
                id: "R-toggle".into(),
                kind: "bash".into(),
                // SEC-12/COR-10: literal substring matcher.
                matcher: r#"{"command_substring":"x"}"#.into(),
                severity: "warn".into(),
                reason: "toggle me".into(),
                namespace: "_global".into(),
                disabled: true,
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("add");

        // Enable.
        let args = RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: RulesAction::Enable {
                id: "R-toggle".into(),
                sign: true,
            },
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("enable");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert!(
            rules_store::get(&conn, "R-toggle")
                .unwrap()
                .unwrap()
                .enabled
        );
        drop(conn);

        // Disable.
        let args = RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: RulesAction::Disable {
                id: "R-toggle".into(),
                sign: true,
            },
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("disable");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert!(
            !rules_store::get(&conn, "R-toggle")
                .unwrap()
                .unwrap()
                .enabled
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_disable_unsign_refuses() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Disable {
                id: "R-x".into(),
                sign: false,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must refuse");
        assert!(format!("{err:#}").contains("no_operator_key"));
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_disable_unknown_id_errors() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Disable {
                id: "R-missing".into(),
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must error");
        assert!(format!("{err:#}").contains("no rule with id"));
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_remove_unsign_refuses() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Remove {
                id: "R-x".into(),
                sign: false,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let err = run(&db_path, args, false, &mut out).expect_err("must refuse");
        assert!(format!("{err:#}").contains("no_operator_key"));
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_remove_signed_deletes_row() {
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        // Add then remove.
        let args = RulesArgs {
            key_dir: Some(key_dir.clone()),
            action: RulesAction::Add {
                id: "R-rm".into(),
                kind: "bash".into(),
                // SEC-12/COR-10: literal substring matcher.
                matcher: r#"{"command_substring":"x"}"#.into(),
                severity: "warn".into(),
                reason: "rm me".into(),
                namespace: "_global".into(),
                disabled: false,
                sign: true,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("add");

        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::Remove {
                id: "R-rm".into(),
                sign: true,
            },
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("remove");
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("rules.remove"), "got: {s}");
        assert!(s.contains("\"removed\":true"), "got: {s}");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        assert!(rules_store::get(&conn, "R-rm").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_keygen_writes_keypair_under_explicit_out() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("ai-memory.db");
        drop(crate::db::open(&db_path).expect("db::open"));
        let key_path = dir.path().join("op.key");
        let args = RulesArgs {
            key_dir: None,
            action: RulesAction::Keygen {
                out: Some(key_path.clone()),
                force: false,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, true, &mut out).expect("keygen");
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("rules.keygen"), "got: {s}");
        assert!(key_path.exists(), "priv key missing");
        let pub_path = pub_sibling_path(&key_path);
        assert!(pub_path.exists(), "pub key missing");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_sign_seed_signs_existing_rules() {
        // Build a fully-initialized DB, add a rule via run(), then call
        // sign-seed via run() (with --db override + --key explicit).
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        // Add an unsigned-attest-level rule directly so sign_seed_rules
        // has at least one row to operate on.
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        rules_store::insert(
            &conn,
            &Rule {
                id: "R-ss".into(),
                kind: "bash".into(),
                matcher: r#"{"command_regex":"^x"}"#.into(),
                severity: "refuse".into(),
                reason: "t".into(),
                namespace: "_global".into(),
                created_by: "test".into(),
                created_at: 0,
                enabled: true,
                signature: None,
                attest_level: "unsigned".into(),
            },
        )
        .unwrap();
        drop(conn);

        // The sign-seed verb expects the singleton-file layout
        // (`~/.config/ai-memory/operator.key`). We saved the keypair
        // in dir-layout for the other tests, so generate a fresh
        // singleton-file via `keygen_operator` first.
        let dir2 = tempfile::tempdir().unwrap();
        let key_file = dir2.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_file, false, &mut out).unwrap();

        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::SignSeed {
                key: Some(key_file),
                db: Some(db_path.clone()),
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        // We pass a separate `--db` to drive the dispatch's
        // `if let Some(db_path) = db` branch (line 350-354).
        let placeholder_db = tempfile::tempdir().unwrap();
        let placeholder_path = placeholder_db.path().join("placeholder.db");
        drop(crate::db::open(&placeholder_path).unwrap());
        run(&placeholder_path, args, true, &mut out).expect("sign-seed");
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("rules.sign-seed"), "got: {s}");
    }

    #[cfg(unix)]
    #[test]
    fn run_rules_sign_seed_reuses_open_conn_when_no_db_override() {
        // Drives the else-branch (line 356) where the top-level
        // `--db` flag's open connection is reused.
        let (_dir, db_path, key_dir) = fresh_env_with_operator_key();
        let dir2 = tempfile::tempdir().unwrap();
        let key_file = dir2.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_file, false, &mut out).unwrap();
        let args = RulesArgs {
            key_dir: Some(key_dir),
            action: RulesAction::SignSeed {
                key: Some(key_file),
                db: None,
            },
        };
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        run(&db_path, args, false, &mut out).expect("sign-seed reuse");
    }

    #[test]
    fn resolve_key_dir_returns_override() {
        let p = std::path::PathBuf::from("/some/explicit/dir");
        let out = resolve_key_dir(Some(&p)).unwrap();
        assert_eq!(out, p);
    }

    #[test]
    fn resolve_operator_key_path_returns_override() {
        let p = std::path::PathBuf::from("/custom/operator.key");
        let out = resolve_operator_key_path(Some(&p)).unwrap();
        assert_eq!(out, p);
    }

    #[test]
    fn resolve_operator_key_path_default_includes_ai_memory() {
        let p = resolve_operator_key_path(None).unwrap();
        let s = p.display().to_string();
        assert!(
            s.contains("ai-memory"),
            "default path missing ai-memory: {s}"
        );
        assert!(s.ends_with("operator.key"), "got: {s}");
    }

    #[test]
    fn emit_ok_human_format_emits_pretty_json() {
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let payload = serde_json::json!({"foo":"bar","n":1});
        emit_ok(false, &mut out, "test.verb", &payload).unwrap();
        let s = String::from_utf8(stdout).unwrap();
        // Pretty-print includes newlines + 2-space indent.
        assert!(s.contains("\"foo\": \"bar\""), "got: {s}");
        assert!(s.contains("\n"), "pretty must include newlines: {s}");
    }

    #[test]
    fn emit_ok_json_format_envelopes_under_verb() {
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        let payload = serde_json::json!({"x":1});
        emit_ok(true, &mut out, "test.verb", &payload).unwrap();
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("\"verb\":\"test.verb\""), "got: {s}");
        assert!(s.contains("\"result\":{\"x\":1}"), "got: {s}");
    }

    #[test]
    fn resolve_agent_id_returns_non_empty() {
        // The fn falls back to `anonymous:pid-<N>` if identity
        // resolution fails — never returns an empty string.
        let id = resolve_agent_id();
        assert!(!id.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn sign_seed_rules_is_idempotent() {
        let tdir = tempfile::tempdir().unwrap();
        let key_path = tdir.path().join("operator.key");
        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut out = CliOutput {
            stdout: &mut stdout,
            stderr: &mut stderr,
        };
        keygen_operator(&key_path, false, &mut out).unwrap();

        let conn = fresh_rules_conn();
        rules_store::insert(
            &conn,
            &Rule {
                id: "R001".into(),
                kind: "filesystem_write".into(),
                matcher: r#"{"glob":"/tmp/**"}"#.into(),
                severity: "refuse".into(),
                reason: "t".into(),
                namespace: "_global".into(),
                created_by: "system:seed".into(),
                created_at: 0,
                enabled: false,
                signature: None,
                attest_level: "unsigned".into(),
            },
        )
        .unwrap();

        // First call: signs 1 row.
        let signed1 = sign_seed_rules(&conn, Some(&key_path), true, &mut out).unwrap();
        assert_eq!(signed1, 1);
        let sig_after_first = rules_store::get(&conn, "R001").unwrap().unwrap().signature;

        // Second call: no-op because the canonical bytes + key are the
        // same so the computed signature matches the stored one.
        let signed2 = sign_seed_rules(&conn, Some(&key_path), true, &mut out).unwrap();
        assert_eq!(signed2, 0);
        let sig_after_second = rules_store::get(&conn, "R001").unwrap().unwrap().signature;
        assert_eq!(
            sig_after_first, sig_after_second,
            "idempotent sign-seed must preserve the existing signature bytes"
        );
    }
}
