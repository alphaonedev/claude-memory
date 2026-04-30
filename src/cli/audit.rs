// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory audit` — operator-facing CLI for the security audit
//! trail (PR-5 of issue #487).
//!
//! Subcommands:
//! - `verify` — walk the configured audit log and assert the hash chain
//!   is intact. Exits non-zero on any mismatch with the precise line
//!   number and failure kind.
//! - `tail` — print recent audit events in JSON or text form.
//! - `path` — print the resolved audit log path. Useful for SIEM
//!   ingestion configuration scripts.

use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(test)]
use std::path::Path;

use anyhow::Result;
use clap::{Args, Subcommand};

use crate::audit::{
    AuditEvent, resolve_audit_path, resolve_audit_path_with_override, verify_chain,
};
use crate::cli::CliOutput;
use crate::config::AppConfig;

#[derive(Args)]
pub struct AuditArgs {
    #[command(subcommand)]
    pub action: AuditAction,
    /// Override the audit log directory. Highest-priority layer in the
    /// resolution ladder (CLI > `AI_MEMORY_AUDIT_DIR` > `[audit] path`
    /// in config.toml > platform default). Refuses world-writable
    /// directories — see `docs/security/audit-trail.md`.
    #[arg(long, global = true, value_name = "PATH")]
    pub audit_dir: Option<std::path::PathBuf>,
}

#[derive(Subcommand)]
pub enum AuditAction {
    /// Verify the hash chain. Exits 0 on success, 2 on mismatch.
    Verify(VerifyArgs),
    /// Print the most recent N events (default 50).
    Tail(TailArgs),
    /// Print the resolved audit log path.
    Path,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Override the configured audit log path.
    #[arg(long)]
    pub path: Option<String>,
    /// Emit a JSON report instead of text.
    #[arg(long, default_value_t = false)]
    pub json: bool,
}

#[derive(Args)]
pub struct TailArgs {
    /// Override the configured audit log path.
    #[arg(long)]
    pub path: Option<String>,
    /// Number of trailing lines to print. Default 50.
    #[arg(long, default_value_t = 50)]
    pub lines: usize,
    /// Filter by `actor.agent_id`.
    #[arg(long)]
    pub actor: Option<String>,
    /// Filter by `target.namespace`.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Filter by `action`.
    #[arg(long)]
    pub action: Option<String>,
    /// Output format: `json` (default) or `text`.
    #[arg(long, default_value = "json")]
    pub format: String,
}

/// `ai-memory audit` entry point. Returns the desired process exit
/// code so the caller can surface a non-zero status from the top-level
/// dispatch without panicking.
pub fn run(args: AuditArgs, app_config: &AppConfig, out: &mut CliOutput<'_>) -> Result<i32> {
    let audit_dir = args.audit_dir.clone();
    match args.action {
        AuditAction::Verify(v) => run_verify(&v, audit_dir.as_deref(), app_config, out),
        AuditAction::Tail(t) => run_tail(&t, audit_dir.as_deref(), app_config, out),
        AuditAction::Path => run_path(audit_dir.as_deref(), app_config, out),
    }
}

/// Resolve the audit log path honouring (in order): explicit per-subcommand
/// `--path` (legacy `VerifyArgs.path` / `TailArgs.path`), the global
/// `--audit-dir` flag, `AI_MEMORY_AUDIT_DIR`, `[audit] path` in
/// config.toml, and the platform default. Falls back to the loose
/// `resolve_audit_path` if any layer above produces an error so the
/// `audit path` subcommand can still print a useful answer when
/// `--audit-dir` is mistyped.
fn resolve_path(
    app_config: &AppConfig,
    cli_audit_dir: Option<&std::path::Path>,
    explicit_per_cmd: Option<&str>,
) -> std::path::PathBuf {
    if let Some(p) = explicit_per_cmd {
        return std::path::PathBuf::from(crate::audit::expand_tilde(p));
    }
    let cfg = app_config.effective_audit();
    if let Ok((p, _src)) = resolve_audit_path_with_override(cli_audit_dir, &cfg) {
        return p;
    }
    resolve_audit_path(&cfg)
}

fn run_verify(
    args: &VerifyArgs,
    cli_audit_dir: Option<&std::path::Path>,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let path = resolve_path(app_config, cli_audit_dir, args.path.as_deref());
    if !path.exists() {
        if args.json {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({
                    "status": "ok",
                    "total_lines": 0,
                    "note": "audit log does not exist (audit may be disabled)",
                    "path": path.display().to_string(),
                })
            )?;
        } else {
            writeln!(
                out.stdout,
                "audit verify: log not present at {} — nothing to check",
                path.display()
            )?;
        }
        return Ok(0);
    }
    let report = verify_chain(&path)?;
    if let Some(failure) = &report.first_failure {
        if args.json {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({
                    "status": "fail",
                    "total_lines": report.total_lines,
                    "failure": {
                        "line_number": failure.line_number,
                        "kind": format!("{:?}", failure.kind),
                        "detail": failure.detail,
                    },
                    "path": path.display().to_string(),
                })
            )?;
        } else {
            writeln!(
                out.stderr,
                "audit verify FAIL at line {}: {:?} — {}",
                failure.line_number, failure.kind, failure.detail
            )?;
        }
        return Ok(2);
    }
    if args.json {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({
                "status": "ok",
                "total_lines": report.total_lines,
                "path": path.display().to_string(),
            })
        )?;
    } else {
        writeln!(
            out.stdout,
            "audit verify OK: {} line(s) verified at {}",
            report.total_lines,
            path.display()
        )?;
    }
    Ok(0)
}

fn run_tail(
    args: &TailArgs,
    cli_audit_dir: Option<&std::path::Path>,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let path = resolve_path(app_config, cli_audit_dir, args.path.as_deref());
    if !path.exists() {
        return Ok(0);
    }
    let f = fs::File::open(&path)?;
    let buf = BufReader::new(f);
    let mut keep: Vec<AuditEvent> = Vec::new();
    for line in buf.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<AuditEvent>(&line) else {
            continue;
        };
        if let Some(actor) = &args.actor
            && !ev.actor.agent_id.contains(actor)
        {
            continue;
        }
        if let Some(ns) = &args.namespace
            && ev.target.namespace != *ns
        {
            continue;
        }
        if let Some(action) = &args.action
            && ev.action.as_str() != action
        {
            continue;
        }
        keep.push(ev);
        if keep.len() > args.lines {
            keep.remove(0);
        }
    }
    let json_format = args.format != "text";
    for ev in &keep {
        if json_format {
            writeln!(out.stdout, "{}", serde_json::to_string(ev)?)?;
        } else {
            writeln!(
                out.stdout,
                "{} seq={} {} {} ns={} id={} outcome={:?}",
                ev.timestamp,
                ev.sequence,
                ev.actor.agent_id,
                ev.action.as_str(),
                ev.target.namespace,
                ev.target.memory_id,
                ev.outcome,
            )?;
        }
    }
    Ok(0)
}

fn run_path(
    cli_audit_dir: Option<&std::path::Path>,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let p = resolve_path(app_config, cli_audit_dir, None);
    writeln!(out.stdout, "{}", p.display())?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::{
        AuditAction as AAct, AuditOutcome, CHAIN_HEAD_PREV_HASH, EventBuilder, actor, target_memory,
    };
    use crate::config::AuditConfig;

    fn write_chained_log(dir: &Path) -> std::path::PathBuf {
        // Build a 3-line chain by hand using the public API; we use
        // the audit module's `init` so emit() produces the lines.
        let path = dir.join("audit.log");
        // Reset the global sink across test runs by spinning a fresh
        // process is impossible; fall back to writing the lines
        // directly.
        let mut prev_hash = CHAIN_HEAD_PREV_HASH.to_string();
        let mut buf = String::new();
        for seq in 1..=3 {
            let ev = make_event(seq, &prev_hash);
            prev_hash = ev.self_hash.clone();
            buf.push_str(&serde_json::to_string(&ev).unwrap());
            buf.push('\n');
        }
        fs::write(&path, buf).unwrap();
        path
    }

    fn make_event(seq: u64, prev: &str) -> AuditEvent {
        let mut ev = AuditEvent {
            schema_version: crate::audit::SCHEMA_VERSION,
            timestamp: format!("2026-04-30T00:00:0{seq}+00:00"),
            sequence: seq,
            actor: actor("ai:test@host:pid-1", "host_fallback", None),
            action: AAct::Store,
            target: target_memory(
                format!("mem-{seq}"),
                "ns-x",
                Some("title".to_string()),
                Some("mid".to_string()),
                None,
            ),
            outcome: AuditOutcome::Allow,
            auth: None,
            session_id: None,
            request_id: None,
            error: None,
            prev_hash: prev.to_string(),
            self_hash: String::new(),
        };
        // Recompute self_hash via the builder helper exposed
        // through serde round-trip in tests.
        let canonical = {
            let mut clone = ev.clone();
            clone.self_hash.clear();
            serde_json::to_string(&clone).unwrap()
        };
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(canonical.as_bytes());
        let bytes = h.finalize();
        let mut s = String::with_capacity(64);
        for b in bytes.iter() {
            s.push_str(&format!("{b:02x}"));
        }
        ev.self_hash = s;
        ev
    }

    #[test]
    fn audit_verify_subcmd_reports_ok_for_valid_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig {
            audit: Some(AuditConfig {
                enabled: Some(true),
                path: Some(p.to_string_lossy().into_owned()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(p.to_string_lossy().into_owned()),
                json: true,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["total_lines"], 3);
    }

    #[test]
    fn audit_verify_subcmd_detects_tampering() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        // Corrupt the second line.
        let mut body = fs::read_to_string(&p).unwrap();
        body = body.replacen("\"sequence\":2", "\"sequence\":99", 1);
        fs::write(&p, body).unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(p.to_string_lossy().into_owned()),
                json: true,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 2, "tampering must produce non-zero exit");
        let s = std::str::from_utf8(&stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["status"], "fail");
    }

    #[test]
    fn audit_verify_subcmd_missing_log_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(tmp.path().join("nope.log").to_string_lossy().into_owned()),
                json: false,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("nothing to check"));
    }

    #[test]
    fn audit_path_subcmd_prints_resolved_path() {
        let cfg = AppConfig {
            audit: Some(AuditConfig {
                path: Some("/var/log/ai-memory/custom.log".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        run_path(None, &cfg, &mut out).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("/var/log/ai-memory/custom.log"));
    }

    #[test]
    fn audit_path_subcmd_honours_audit_dir_flag() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        run_path(Some(tmp.path()), &cfg, &mut out).unwrap();
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(
            s.contains(tmp.path().to_string_lossy().as_ref()),
            "expected audit-dir override to surface in `audit path` output: {s}"
        );
        assert!(s.contains("audit.log"));
    }

    // Compile-time guardrail — make sure EventBuilder is visible from
    // this module (it's the public emit-API).
    #[allow(dead_code)]
    fn _builder_is_visible() {
        let _ = EventBuilder::new(
            AAct::Store,
            actor("a", "explicit", None),
            target_memory("m", "ns", None, None, None),
        );
    }

    // ------------------------------------------------------------------
    // PR-9e coverage uplift (issue #487): exercise the top-level `run`
    // dispatcher and the `run_tail` body. Pre-existing tests jumped
    // straight to `run_verify` / `run_path`; the audit dispatcher arm
    // for `audit tail` had no coverage at all.
    // ------------------------------------------------------------------

    #[test]
    fn audit_run_dispatches_to_verify_arm() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = AuditArgs {
            action: AuditAction::Verify(VerifyArgs {
                path: Some(p.to_string_lossy().into_owned()),
                json: true,
            }),
            audit_dir: None,
        };
        let exit = run(args, &cfg, &mut out).unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("\"status\":\"ok\""), "got: {s}");
    }

    #[test]
    fn audit_run_dispatches_to_tail_arm() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = AuditArgs {
            action: AuditAction::Tail(TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 10,
                actor: None,
                namespace: None,
                action: None,
                format: "json".to_string(),
            }),
            audit_dir: None,
        };
        let exit = run(args, &cfg, &mut out).unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        let count = s.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(count, 3, "expected 3 events from chain, got {count}: {s}");
    }

    #[test]
    fn audit_run_dispatches_to_path_arm() {
        let cfg = AppConfig {
            audit: Some(AuditConfig {
                path: Some("/var/log/ai-memory/from-run.log".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let args = AuditArgs {
            action: AuditAction::Path,
            audit_dir: None,
        };
        let exit = run(args, &cfg, &mut out).unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("from-run.log"), "got: {s}");
    }

    #[test]
    fn audit_tail_subcmd_returns_last_n_events_in_text_format() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 2,
                actor: None,
                namespace: None,
                action: None,
                format: "text".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        // Text format includes "seq=" prefix per line.
        assert!(s.contains("seq="), "expected text format: {s}");
        let count = s.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(count, 2, "lines arg must cap output at 2: {s}");
    }

    #[test]
    fn audit_tail_subcmd_emits_json_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 50,
                actor: None,
                namespace: None,
                action: None,
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        // First line must parse as JSON.
        let first = s.lines().next().expect("at least one line");
        let v: serde_json::Value = serde_json::from_str(first).expect("json");
        assert_eq!(v["schema_version"], 1);
        assert!(v.get("self_hash").is_some());
    }

    #[test]
    fn audit_tail_subcmd_filters_by_actor() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 50,
                // Filter that does not match (write_chained_log uses
                // "ai:test@host:pid-1") — every event must be dropped.
                actor: Some("nope-not-in-log".to_string()),
                namespace: None,
                action: None,
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.is_empty(), "actor filter must drop all events: {s}");
    }

    #[test]
    fn audit_tail_subcmd_filters_by_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 50,
                actor: None,
                // Mismatched namespace — events use "ns-x" exactly.
                namespace: Some("not-ns-x".to_string()),
                action: None,
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        assert!(stdout.is_empty(), "namespace filter must drop everything");
    }

    #[test]
    fn audit_tail_subcmd_filters_by_action_string() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        // The chained log uses Store actions. Filter on "delete" — drop them.
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 50,
                actor: None,
                namespace: None,
                action: Some("delete".to_string()),
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        assert!(
            stdout.is_empty(),
            "action=delete must drop all store events"
        );
    }

    #[test]
    fn audit_tail_subcmd_returns_zero_when_log_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(
                    tmp.path()
                        .join("does-not-exist.log")
                        .to_string_lossy()
                        .into_owned(),
                ),
                lines: 50,
                actor: None,
                namespace: None,
                action: None,
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        assert!(stdout.is_empty());
    }

    #[test]
    fn audit_tail_subcmd_skips_malformed_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        // Append a malformed line; tail must skip it (continue) and
        // still emit the valid 3 events.
        let mut body = fs::read_to_string(&p).unwrap();
        body.push_str("not-valid-json\n\n");
        fs::write(&p, body).unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_tail(
            &TailArgs {
                path: Some(p.to_string_lossy().into_owned()),
                lines: 50,
                actor: None,
                namespace: None,
                action: None,
                format: "json".to_string(),
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        let count = s.lines().filter(|l| !l.is_empty()).count();
        assert_eq!(
            count, 3,
            "must skip malformed line and keep the 3 good events"
        );
    }

    #[test]
    fn audit_verify_subcmd_missing_log_emits_json_when_flag_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(tmp.path().join("nope.log").to_string_lossy().into_owned()),
                // JSON-format the missing-log response: exercises the
                // `args.json` branch of the missing-log early return.
                json: true,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["status"], "ok");
        assert_eq!(v["total_lines"], 0);
        assert!(v["note"].as_str().unwrap().contains("does not exist"));
    }

    #[test]
    fn audit_verify_subcmd_text_failure_writes_to_stderr() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        // Tamper to force a verify failure.
        let mut body = fs::read_to_string(&p).unwrap();
        body = body.replacen("\"sequence\":2", "\"sequence\":99", 1);
        fs::write(&p, body).unwrap();
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(p.to_string_lossy().into_owned()),
                // text path: writes failure to stderr instead of stdout
                json: false,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 2);
        let serr = std::str::from_utf8(&stderr).unwrap();
        assert!(
            serr.contains("audit verify FAIL"),
            "expected text-format failure on stderr: {serr}"
        );
    }

    #[test]
    fn audit_verify_subcmd_text_success_writes_to_stdout() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write_chained_log(tmp.path());
        let cfg = AppConfig::default();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        let exit = run_verify(
            &VerifyArgs {
                path: Some(p.to_string_lossy().into_owned()),
                // text-format success path
                json: false,
            },
            None,
            &cfg,
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("audit verify OK"), "got: {s}");
        assert!(s.contains("3 line(s) verified"));
    }
}
