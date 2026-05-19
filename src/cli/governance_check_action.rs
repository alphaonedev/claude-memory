// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 issue #863 — `ai-memory governance check-action` CLI
//! subcommand. Shell-side parity surface for the MCP tool
//! `memory_check_agent_action`. Operators can dry-run any substrate
//! agent-action rule (R001-R004 plus any operator-added rule) from
//! a terminal without driving JSON-RPC over stdio.
//!
//! ## Wire shape
//!
//! ```text
//! ai-memory governance check-action \
//!     --kind <bash|filesystem_write|network_request|process_spawn|custom> \
//!     [--command <str>]   (bash)
//!     [--path <str>]      (filesystem_write)
//!     [--host <str>]      (network_request)
//!     [--binary <str>]    (process_spawn)
//!     [--custom-kind <str>] (custom)
//!     [--agent-id <str>]
//!     [--json]
//! ```
//!
//! Defaults `agent_id` to the same `anonymous:mcp` sentinel the MCP
//! handler uses so the audit trail is symmetric across surfaces.
//!
//! ## Output
//!
//! - Human (default): one line per outcome
//!   `Allow` / `Refuse: <rule_id> — <reason>` / `Warn: <rule_id> — <reason>`.
//! - `--json`: the wire envelope from
//!   [`crate::mcp::tools::check_agent_action::run_check`] unchanged
//!   (`{"decision":{...},"kind":"...","agent_id":"..."}`).
//!
//! ## DRY contract
//!
//! Every per-kind validation and the rule-engine call route through
//! [`crate::mcp::tools::check_agent_action::build_action`] and
//! [`crate::mcp::tools::check_agent_action::run_check`]. No business
//! logic lives here — this module is a clap arg-parser plus an output
//! formatter.

use anyhow::{Context, Result};
use clap::Args;
use serde_json::Value;

use crate::cli::CliOutput;
use crate::mcp::tools::check_agent_action::{
    DEFAULT_AGENT_ID, build_action as build_agent_action, run_check,
};

/// CLI args for `ai-memory governance check-action`. The per-kind
/// fields are optional at the clap layer; the substrate shared helper
/// validates which fields are required for the supplied `--kind`.
#[derive(Args, Debug, Clone)]
pub struct CheckActionArgs {
    /// AgentAction kind — one of `bash`, `filesystem_write`,
    /// `network_request`, `process_spawn`, `custom`. Mirrors the
    /// `governance_rules.kind` enum.
    #[arg(long, value_name = "KIND")]
    pub kind: String,

    /// Shell command (required when `--kind bash`).
    #[arg(long, value_name = "COMMAND")]
    pub command: Option<String>,

    /// Filesystem path (required when `--kind filesystem_write`).
    #[arg(long, value_name = "PATH")]
    pub path: Option<String>,

    /// Host (required when `--kind network_request`).
    #[arg(long, value_name = "HOST")]
    pub host: Option<String>,

    /// Resolved binary name (required when `--kind process_spawn`).
    #[arg(long, value_name = "BINARY")]
    pub binary: Option<String>,

    /// Inner custom kind (required when `--kind custom`).
    #[arg(long = "custom-kind", value_name = "KIND")]
    pub custom_kind: Option<String>,

    /// Optional agent id stamped into the audit row. Defaults to the
    /// same `anonymous:mcp` sentinel the MCP handler uses.
    #[arg(long = "agent-id", value_name = "ID")]
    pub agent_id: Option<String>,

    /// Emit the raw JSON envelope (same shape as the MCP tool) instead
    /// of the human-readable verdict line.
    #[arg(long)]
    pub json: bool,
}

impl CheckActionArgs {
    /// Convert the CLI arg-bag into the JSON object shape the shared
    /// [`build_agent_action`] helper expects.
    fn to_arguments(&self) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("kind".to_string(), Value::String(self.kind.clone()));
        if let Some(v) = &self.command {
            obj.insert("command".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.path {
            obj.insert("path".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.host {
            obj.insert("host".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.binary {
            obj.insert("binary".to_string(), Value::String(v.clone()));
        }
        if let Some(v) = &self.custom_kind {
            obj.insert("custom_kind".to_string(), Value::String(v.clone()));
        }
        Value::Object(obj)
    }
}

/// Dispatch entry called from the daemon-runtime `GovernanceAction`
/// match arm.
///
/// # Errors
///
/// - The rules DB at `db_path` cannot be opened.
/// - The shared `build_action` rejects the supplied kind / fields
///   (e.g. `--kind filesystem_write` without `--path`).
/// - The shared `run_check` call returns an error (rules-table SQL
///   failure, audit emit failure).
/// - `--json` mode and `serde_json` cannot serialise the envelope
///   (in practice never happens with the shapes used here).
pub fn run(
    db_path: &std::path::Path,
    args: &CheckActionArgs,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = rusqlite::Connection::open(db_path)
        .with_context(|| format!("governance check-action: open db at {}", db_path.display()))?;

    let arguments = args.to_arguments();
    let action = build_agent_action(&args.kind, &arguments)
        .map_err(|e| anyhow::anyhow!("governance check-action: {e}"))?;
    let agent_id = args
        .agent_id
        .clone()
        .unwrap_or_else(|| DEFAULT_AGENT_ID.to_string());

    let envelope = run_check(&conn, &agent_id, &args.kind, &action)
        .map_err(|e| anyhow::anyhow!("governance check-action: {e}"))?;

    if args.json {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&envelope)
                .context("governance check-action: serialise JSON envelope")?
        )?;
        return Ok(());
    }

    // Human-readable verdict line. The `decision` sub-object follows the
    // serde-tagged shape from `governance::agent_action::Decision`:
    // {"decision": "allow"} / {"decision": "refuse", "rule_id": "...", "reason": "..."}
    let decision = envelope.get("decision").cloned().unwrap_or(Value::Null);
    let verdict = decision
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    match verdict {
        "allow" => writeln!(out.stdout, "Allow")?,
        "refuse" => {
            let rule_id = decision
                .get("rule_id")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let reason = decision
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("?");
            writeln!(out.stdout, "Refuse: {rule_id} — {reason}")?;
        }
        "warn" => {
            let rule_id = decision
                .get("rule_id")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let reason = decision
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("?");
            writeln!(out.stdout, "Warn: {rule_id} — {reason}")?;
        }
        other => writeln!(out.stdout, "Unknown verdict: {other}")?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::NamedTempFile;

    fn seed_rules_db() -> NamedTempFile {
        let tmp = NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(tmp.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );
             CREATE TABLE signed_events (
                 id TEXT PRIMARY KEY,
                 agent_id TEXT NOT NULL,
                 event_type TEXT NOT NULL,
                 payload_hash BLOB NOT NULL,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned',
                 timestamp TEXT NOT NULL,
                 prev_hash BLOB,
                 sequence INTEGER
             );",
        )
        .unwrap();
        // Seed R001-style filesystem_write rule (enabled = 1, no signature
        // required because tests force_no_operator_pubkey_for_test below).
        conn.execute(
            "INSERT INTO governance_rules (id, kind, matcher, severity, reason, \
             namespace, created_by, created_at, enabled, signature, attest_level) \
             VALUES (?1, ?2, ?3, 'refuse', ?4, '_global', 'test', 0, 1, NULL, 'unsigned')",
            params![
                "R001",
                "filesystem_write",
                r#"{"glob":"/tmp/**"}"#,
                "no /tmp writes",
            ],
        )
        .unwrap();
        tmp
    }

    #[test]
    fn refuses_filesystem_write_to_tmp() {
        let _no_pubkey = crate::governance::rules_store::force_no_operator_pubkey_for_test();
        let tmp = seed_rules_db();
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let args = CheckActionArgs {
            kind: "filesystem_write".into(),
            command: None,
            path: Some("/tmp/foo.txt".into()),
            host: None,
            binary: None,
            custom_kind: None,
            agent_id: None,
            json: true,
        };
        run(tmp.path(), &args, &mut out).unwrap();
        let stdout = String::from_utf8(so).unwrap();
        let v: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(v["decision"]["decision"], "refuse");
        assert_eq!(v["decision"]["rule_id"], "R001");
    }

    #[test]
    fn allows_filesystem_write_outside_tmp() {
        let _no_pubkey = crate::governance::rules_store::force_no_operator_pubkey_for_test();
        let tmp = seed_rules_db();
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let args = CheckActionArgs {
            kind: "filesystem_write".into(),
            command: None,
            path: Some("/home/user/ok.txt".into()),
            host: None,
            binary: None,
            custom_kind: None,
            agent_id: None,
            json: false,
        };
        run(tmp.path(), &args, &mut out).unwrap();
        let stdout = String::from_utf8(so).unwrap();
        assert!(stdout.trim() == "Allow", "got: {stdout}");
    }

    #[test]
    fn missing_required_field_errors() {
        let tmp = seed_rules_db();
        let mut so = Vec::<u8>::new();
        let mut se = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut so, &mut se);
        let args = CheckActionArgs {
            kind: "filesystem_write".into(),
            command: None,
            path: None,
            host: None,
            binary: None,
            custom_kind: None,
            agent_id: None,
            json: false,
        };
        let err = run(tmp.path(), &args, &mut out).unwrap_err();
        assert!(err.to_string().contains("path"), "got: {err}");
    }
}
