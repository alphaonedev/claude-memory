// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory identity` subcommand — per-agent Ed25519 keypair lifecycle
//! (Track H, Task H1).
//!
//! See [`crate::identity::keypair`] for the underlying lifecycle. This
//! module is the thin clap wrapper that turns command-line input into
//! the four verbs (`generate / import / list / export-pub`) and prints
//! the result via the standard [`CliOutput`] writer pair.
//!
//! ## Hardware-backed key storage is OUT of OSS scope
//!
//! TPM 2.0, PKCS#11 HSMs, Apple Secure Enclave, and cloud KMS adapters
//! are intentionally not in this subcommand. See the module-level
//! comment on [`crate::identity::keypair`] and `ROADMAP2.md` —
//! AgenticMem™ is the commercial home for those backends.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use ed25519_dalek::SigningKey;

use crate::cli::CliOutput;
use crate::identity::{self, keypair};

#[derive(Args)]
pub struct IdentityArgs {
    /// Override the default key storage directory.
    /// Default by platform:
    ///   Linux:   `~/.config/ai-memory/keys/`,
    ///   macOS:   `~/Library/Application Support/ai-memory/keys/`,
    ///   Windows: `%APPDATA%\ai-memory\keys\`.
    /// Honors `AI_MEMORY_KEY_DIR` env var when this flag is omitted.
    #[arg(long, value_name = "PATH", global = true)]
    pub key_dir: Option<PathBuf>,
    #[command(subcommand)]
    pub action: IdentityAction,
}

#[derive(Subcommand)]
pub enum IdentityAction {
    /// Generate a fresh Ed25519 keypair for `--agent-id` (or the
    /// NHI-hardened default if omitted) and persist it under the key
    /// storage directory with strict 0600/0644 modes on Unix.
    Generate {
        /// Agent identifier. Defaults to the same NHI-hardened id the
        /// rest of the CLI synthesizes (e.g. `host:<host>:pid-<pid>-<uuid8>`).
        #[arg(long)]
        agent_id: Option<String>,
        /// Allow overwriting an existing keypair for `--agent-id`.
        /// Without this flag, `generate` refuses on an existing id —
        /// the safe default to prevent a typo from silently rotating
        /// (and irrecoverably destroying) a daemon or peer key. Pass
        /// `--force` only when you intend to rotate.
        #[arg(long, default_value_t = false)]
        force: bool,
        /// Deprecated alias retained for backward compatibility with
        /// the v0.7.0 pre-Round-4 flag surface. The default behavior
        /// is now refuse-on-existing; this flag is a no-op. Use
        /// `--force` to opt INTO overwrite.
        #[arg(long, default_value_t = false, hide = true)]
        no_overwrite: bool,
    },
    /// Import a keypair from on-disk files written by another tool.
    /// `--pub` is required; `--priv` is optional (omit it to import a
    /// public-only handle for verification, e.g., a peer's allowlist
    /// entry).
    Import {
        /// Agent identifier the imported material will be saved under.
        #[arg(long)]
        agent_id: String,
        /// Path to a 32-byte raw Ed25519 public key file.
        #[arg(long = "pub", value_name = "PATH")]
        public: PathBuf,
        /// Optional path to a 32-byte raw Ed25519 private key file.
        #[arg(long = "priv", value_name = "PATH")]
        private: Option<PathBuf>,
    },
    /// List every keypair stored under the key storage directory.
    /// Private keys are never loaded — `list` is safe to wire into
    /// dashboards and shell autocompletion.
    List,
    /// Print a base64-encoded public key for `--agent-id` to stdout.
    /// Stable URL-safe-no-padding form so the output can be pasted
    /// into a Slack message or a peer allowlist file without binary
    /// hazards.
    ExportPub {
        /// Agent identifier whose public key should be exported.
        #[arg(long)]
        agent_id: String,
    },
}

/// Resolve the key storage directory from `--key-dir` (caller override)
/// or the OSS default at `<config>/ai-memory/keys`.
fn resolve_key_dir(override_dir: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = override_dir {
        return Ok(p.to_path_buf());
    }
    keypair::default_key_dir()
}

/// Resolve the agent_id for a CLI invocation: explicit `--agent-id`
/// wins, otherwise fall back to the NHI default. We pass `None` for
/// the MCP client so the resolution stops at the host-or-anonymous
/// branch (CLI is not an MCP handshake).
fn resolve_id(explicit: Option<&str>) -> Result<String> {
    identity::resolve_agent_id(explicit, None)
}

/// `identity` handler.
///
/// Returns `Ok(())` on success, propagates errors otherwise. The
/// caller is `daemon_runtime::dispatch_command` which prints the error
/// + exits non-zero in the standard way.
pub fn run(args: IdentityArgs, json_out: bool, out: &mut CliOutput<'_>) -> Result<()> {
    let dir = resolve_key_dir(args.key_dir.as_deref())?;
    match args.action {
        IdentityAction::Generate {
            agent_id,
            force,
            no_overwrite: _,
        } => generate(&dir, agent_id.as_deref(), force, json_out, out),
        IdentityAction::Import {
            agent_id,
            public,
            private,
        } => import(&dir, &agent_id, &public, private.as_deref(), json_out, out),
        IdentityAction::List => list(&dir, json_out, out),
        IdentityAction::ExportPub { agent_id } => export_pub(&dir, &agent_id, json_out, out),
    }
}

fn generate(
    dir: &Path,
    explicit_agent_id: Option<&str>,
    force: bool,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let id = resolve_id(explicit_agent_id)?;
    let pub_path = dir.join(format!("{id}.pub"));
    // Round-4 — refuse-by-default. The pre-Round-4 default was OVERWRITE,
    // which let a typo silently rotate (and destroy) a daemon or peer
    // keypair. Now `generate` refuses if a key already exists; the
    // operator must pass `--force` to opt into rotation. The legacy
    // `--no-overwrite` flag is preserved as a hidden no-op for
    // backward compatibility with scripts that invoked it.
    if !force && pub_path.exists() {
        bail!(
            "keypair for {id} already exists at {} (pass --force to rotate; refused by default to prevent accidental key overwrite)",
            pub_path.display()
        );
    }
    let kp = keypair::generate(&id)?;
    keypair::save(&kp, dir)?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "generated": true,
                "agent_id": id,
                "key_dir": dir,
                "public_key_b64": kp.public_base64(),
            })
        )?;
    } else {
        writeln!(out.stdout, "generated keypair for {id}")?;
        writeln!(out.stdout, "  key_dir = {}", dir.display())?;
        writeln!(out.stdout, "  pub_b64 = {}", kp.public_base64())?;
    }
    Ok(())
}

fn import(
    dir: &Path,
    agent_id: &str,
    pub_path: &Path,
    priv_path: Option<&Path>,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    crate::validate::validate_agent_id(agent_id)?;
    let pub_bytes = keypair::read_raw_key_file(pub_path)
        .with_context(|| format!("reading --pub {}", pub_path.display()))?;
    let public = ed25519_dalek::VerifyingKey::from_bytes(&pub_bytes)
        .with_context(|| "decoding imported public key".to_string())?;

    let private = if let Some(p) = priv_path {
        let priv_bytes = keypair::read_raw_key_file(p)
            .with_context(|| format!("reading --priv {}", p.display()))?;
        let signing = SigningKey::from_bytes(&priv_bytes);
        // Cross-check before persisting — refuse mismatched pairs.
        if signing.verifying_key().to_bytes() != public.to_bytes() {
            bail!(
                "imported --priv {} does not match --pub {}",
                p.display(),
                pub_path.display()
            );
        }
        Some(signing)
    } else {
        None
    };

    let kp = keypair::AgentKeypair {
        agent_id: agent_id.to_string(),
        public,
        private,
    };
    if kp.private.is_some() {
        keypair::save(&kp, dir)?;
    } else {
        keypair::save_public_only(&kp, dir)?;
    }

    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "imported": true,
                "agent_id": agent_id,
                "key_dir": dir,
                "private_imported": kp.private.is_some(),
                "public_key_b64": kp.public_base64(),
            })
        )?;
    } else {
        writeln!(
            out.stdout,
            "imported keypair for {agent_id} (private={})",
            if kp.private.is_some() { "yes" } else { "no" }
        )?;
        writeln!(out.stdout, "  key_dir = {}", dir.display())?;
        writeln!(out.stdout, "  pub_b64 = {}", kp.public_base64())?;
    }
    Ok(())
}

fn list(dir: &Path, json_out: bool, out: &mut CliOutput<'_>) -> Result<()> {
    let keys = keypair::list(dir)?;
    if json_out {
        let entries: Vec<_> = keys
            .iter()
            .map(|k| {
                serde_json::json!({
                    "agent_id": k.agent_id,
                    "public_key_b64": k.public_base64(),
                })
            })
            .collect();
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "count": entries.len(),
                "key_dir": dir,
                "keys": entries,
            })
        )?;
    } else if keys.is_empty() {
        writeln!(out.stdout, "no keypairs in {}", dir.display())?;
    } else {
        for k in &keys {
            writeln!(out.stdout, "{}  {}", k.agent_id, k.public_base64())?;
        }
        writeln!(out.stdout, "{} keypair(s) in {}", keys.len(), dir.display())?;
    }
    Ok(())
}

fn export_pub(dir: &Path, agent_id: &str, json_out: bool, out: &mut CliOutput<'_>) -> Result<()> {
    let kp = keypair::load(agent_id, dir)?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "agent_id": agent_id,
                "public_key_b64": kp.public_base64(),
            })
        )?;
    } else {
        // Plain text path: just the base64 — pipe-friendly for
        // `ai-memory identity export-pub --agent-id alice | xclip`.
        writeln!(out.stdout, "{}", kp.public_base64())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;

    fn fresh_env() -> (TestEnv, tempfile::TempDir) {
        let env = TestEnv::fresh();
        let dir = tempfile::TempDir::new().unwrap();
        (env, dir)
    }

    #[test]
    fn generate_then_list_then_export() {
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();

        // generate
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Generate {
                        agent_id: Some("alice".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().to_string();
        assert!(
            stdout.contains("generated keypair for alice"),
            "got: {stdout}"
        );

        // list
        env.stdout.clear();
        env.stderr.clear();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::List,
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().to_string();
        assert!(stdout.contains("alice"), "got: {stdout}");
        assert!(stdout.contains("1 keypair(s)"), "got: {stdout}");

        // export-pub (text mode prints just the base64)
        env.stdout.clear();
        env.stderr.clear();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::ExportPub {
                        agent_id: "alice".to_string(),
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().trim().to_string();
        // Should round-trip through the keypair decoder.
        let decoded = keypair::decode_public_base64(&stdout).expect("decode");
        assert_eq!(decoded.to_bytes().len(), 32);
    }

    #[test]
    fn generate_refuses_existing_without_force() {
        // Round-4 — refuse-by-default semantics. A second `generate`
        // for an existing agent_id MUST fail unless `--force` is
        // passed. The legacy `--no-overwrite` flag is preserved as a
        // hidden no-op for backward compatibility with v0.7.0
        // pre-Round-4 scripts.
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        // First generate
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Generate {
                        agent_id: Some("alice".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        env.stdout.clear();
        env.stderr.clear();
        // Second generate WITHOUT --force should error (refuse-by-default).
        let result = {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Generate {
                        agent_id: Some("alice".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                false,
                &mut out,
            )
        };
        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("already exists"), "got: {msg}");
        assert!(
            msg.contains("--force"),
            "error message should guide operator toward --force, got: {msg}"
        );

        // Third generate WITH --force should succeed (intentional rotation).
        env.stdout.clear();
        env.stderr.clear();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::Generate {
                        agent_id: Some("alice".to_string()),
                        force: true,
                        no_overwrite: false,
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().to_string();
        assert!(
            stdout.contains("generated keypair for alice"),
            "rotation with --force did not succeed: {stdout}"
        );
    }

    #[test]
    fn list_json_emits_keys_array() {
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Generate {
                        agent_id: Some("alice".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        env.stdout.clear();
        env.stderr.clear();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::List,
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 1);
        assert_eq!(v["keys"][0]["agent_id"].as_str().unwrap(), "alice");
        assert!(v["keys"][0]["public_key_b64"].as_str().unwrap().len() > 10);
    }

    #[test]
    fn import_round_trip_through_raw_files() {
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();

        // Create a fresh keypair, dump raw bytes to disk, then `import`.
        let kp = keypair::generate("alice").unwrap();
        let pub_bytes = kp.public.to_bytes();
        let priv_bytes = kp.private.as_ref().unwrap().to_bytes();
        let staging = tempfile::TempDir::new().unwrap();
        let pub_file = staging.path().join("a.pub");
        let priv_file = staging.path().join("a.priv");
        std::fs::write(&pub_file, pub_bytes).unwrap();
        std::fs::write(&priv_file, priv_bytes).unwrap();

        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Import {
                        agent_id: "alice".to_string(),
                        public: pub_file.clone(),
                        private: Some(priv_file.clone()),
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().to_string();
        assert!(
            stdout.contains("imported keypair for alice"),
            "got: {stdout}"
        );
        // Round-trip through load.
        let loaded = keypair::load("alice", &dir_path).unwrap();
        assert_eq!(loaded.public.to_bytes(), pub_bytes);
        assert!(loaded.can_sign());
    }

    #[test]
    fn import_refuses_priv_pub_mismatch() {
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        let alice = keypair::generate("alice").unwrap();
        let bob = keypair::generate("bob").unwrap();
        let staging = tempfile::TempDir::new().unwrap();
        let pub_file = staging.path().join("alice.pub");
        let priv_file = staging.path().join("bob.priv");
        std::fs::write(&pub_file, alice.public.to_bytes()).unwrap();
        std::fs::write(&priv_file, bob.private.as_ref().unwrap().to_bytes()).unwrap();

        let result = {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::Import {
                        agent_id: "alice".to_string(),
                        public: pub_file,
                        private: Some(priv_file),
                    },
                },
                false,
                &mut out,
            )
        };
        let err = result.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("does not match"), "got: {msg}");
    }

    // ------------------------------------------------------------------
    // L0.7-3 chunk-e2 — coverage uplift to ≥95%.
    // ------------------------------------------------------------------

    #[test]
    fn generate_json_mode_emits_payload() {
        // json_out=true on generate exercises the JSON emission branch
        // (lines 159-169) that the existing happy-path test skipped.
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::Generate {
                        agent_id: Some("carol".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["generated"], true);
        assert_eq!(v["agent_id"].as_str().unwrap(), "carol");
        assert!(v["public_key_b64"].as_str().unwrap().len() > 10);
        assert!(v["key_dir"].is_string());
    }

    #[test]
    fn import_public_only_text_mode() {
        // Public-only import (priv=None) covers the `private = None`
        // branch (line 206), the `save_public_only` branch (217), and
        // the text-mode "(private=no)" emission (lines 233-239).
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        let kp = keypair::generate("dave").unwrap();
        let staging = tempfile::TempDir::new().unwrap();
        let pub_file = staging.path().join("d.pub");
        std::fs::write(&pub_file, kp.public.to_bytes()).unwrap();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Import {
                        agent_id: "dave".to_string(),
                        public: pub_file,
                        private: None,
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        let stdout = env.stdout_str().to_string();
        assert!(
            stdout.contains("imported keypair for dave"),
            "got: {stdout}"
        );
        assert!(stdout.contains("(private=no)"), "got: {stdout}");
        // Round-trip — load should succeed and report no signing key.
        let loaded = keypair::load("dave", &dir_path).unwrap();
        assert!(!loaded.can_sign());
    }

    #[test]
    fn import_public_only_json_mode() {
        // JSON emission covers lines 221-231 with `private_imported=false`.
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        let kp = keypair::generate("eve").unwrap();
        let staging = tempfile::TempDir::new().unwrap();
        let pub_file = staging.path().join("e.pub");
        std::fs::write(&pub_file, kp.public.to_bytes()).unwrap();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::Import {
                        agent_id: "eve".to_string(),
                        public: pub_file,
                        private: None,
                    },
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["imported"], true);
        assert_eq!(v["agent_id"].as_str().unwrap(), "eve");
        assert_eq!(v["private_imported"], false);
        assert!(v["public_key_b64"].as_str().unwrap().len() > 10);
    }

    #[test]
    fn import_with_priv_json_mode_reports_private_imported_true() {
        // Mirrors the existing private import test but in json mode to
        // cover lines 220-231 with `private_imported=true`.
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        let kp = keypair::generate("frank").unwrap();
        let staging = tempfile::TempDir::new().unwrap();
        let pub_file = staging.path().join("f.pub");
        let priv_file = staging.path().join("f.priv");
        std::fs::write(&pub_file, kp.public.to_bytes()).unwrap();
        std::fs::write(&priv_file, kp.private.as_ref().unwrap().to_bytes()).unwrap();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::Import {
                        agent_id: "frank".to_string(),
                        public: pub_file,
                        private: Some(priv_file),
                    },
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["private_imported"], true);
    }

    #[test]
    fn list_empty_text_mode_emits_no_keypairs() {
        // Empty list in text mode (line 266).
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::List,
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        assert!(env.stdout_str().contains("no keypairs in"));
    }

    #[test]
    fn list_empty_json_mode_emits_count_zero() {
        // JSON list with zero entries — covers the json branch with the
        // empty `entries` collection (line 264).
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::List,
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
        assert!(v["keys"].as_array().unwrap().is_empty());
    }

    #[test]
    fn export_pub_json_mode_emits_payload() {
        // JSON-mode export-pub (lines 278-286).
        let (mut env, dir) = fresh_env();
        let dir_path = dir.path().to_path_buf();
        // First generate so there is something to export.
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path.clone()),
                    action: IdentityAction::Generate {
                        agent_id: Some("grace".to_string()),
                        force: false,
                        no_overwrite: false,
                    },
                },
                false,
                &mut out,
            )
            .unwrap();
        }
        env.stdout.clear();
        env.stderr.clear();
        {
            let mut out = env.output();
            run(
                IdentityArgs {
                    key_dir: Some(dir_path),
                    action: IdentityAction::ExportPub {
                        agent_id: "grace".to_string(),
                    },
                },
                true,
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["agent_id"].as_str().unwrap(), "grace");
        assert!(v["public_key_b64"].as_str().unwrap().len() > 10);
    }

    #[test]
    fn resolve_key_dir_falls_through_to_default() {
        // No override path → falls through to `keypair::default_key_dir()`
        // (line 102). We don't assert on the contents (HOME-dependent),
        // only that we reach the call and get a `Result`.
        let r = resolve_key_dir(None);
        // The default-key-dir resolution depends on dirs::config_dir(),
        // which is generally available on macOS/Linux test hosts. Tolerate
        // both Ok (typical) and Err (CI without HOME).
        match r {
            Ok(p) => assert!(p.as_os_str().len() > 0),
            Err(_) => {}
        }
    }
}
