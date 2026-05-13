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

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use ed25519_dalek::Signer;
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
            let signing_key = load_operator_signing_key(&key_dir)?;
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
            let signing_key = load_operator_signing_key(&key_dir)?;
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
            let signing_key = load_operator_signing_key(&key_dir)?;
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
            let _ = load_operator_signing_key(&key_dir)?;
            let removed = rules_store::remove(&conn, &id)?;
            let payload = serde_json::json!({ "id": id, "removed": removed });
            emit_ok(json, out, "rules.remove", &payload)?;
            Ok(())
        }
    }
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
fn load_operator_signing_key(key_dir: &std::path::Path) -> Result<ed25519_dalek::SigningKey> {
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
}
