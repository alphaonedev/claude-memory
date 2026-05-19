// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 QW-3 — `ai-memory offload` and `ai-memory deref` CLI commands.
//!
//! Substrate-only wrappers around [`crate::offload::ContextOffloader`].
//! v0.8.0 short-term-context-compression will layer the auto-cadence
//! and Mermaid-canvas trigger paths on top.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};

use crate::cli::CliOutput;
use crate::offload::{ContextOffloader, OffloadConfig};
use crate::storage as db;

#[derive(Args)]
pub struct OffloadArgs {
    /// File whose contents will be offloaded. Pass `-` to read stdin.
    pub file: String,
    /// Namespace the blob lives under. Defaults to `auto` so a short
    /// invocation still records a sensible value.
    #[arg(long)]
    pub namespace: Option<String>,
    /// Optional TTL (seconds). Omit for permanent storage.
    #[arg(long)]
    pub ttl_seconds: Option<u64>,
    /// Override the storing `agent_id`. Defaults to the standard
    /// resolution chain.
    #[arg(long)]
    pub agent_id: Option<String>,
    /// Emit JSON instead of a human-readable line.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct DerefArgs {
    /// The `ref_id` returned by a prior `offload`.
    pub ref_id: String,
    /// Optional output path; otherwise content is written to stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
    /// Emit a JSON envelope alongside the content (suppresses raw
    /// content on stdout when `--out` is also passed).
    #[arg(long)]
    pub json: bool,
}

fn read_input(file: &str) -> Result<String> {
    if file == "-" {
        use std::io::Read;
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("read stdin")?;
        Ok(s)
    } else {
        std::fs::read_to_string(file).with_context(|| format!("read {file}"))
    }
}

/// Resolve `agent_id` against the CLI override, falling back to the
/// project-wide resolution chain (env / hostname / anonymous).
fn resolve_agent_id(override_value: Option<&str>) -> Result<String> {
    if let Some(value) = override_value {
        return Ok(value.to_string());
    }
    crate::identity::resolve_agent_id(None, None)
}

/// `ai-memory offload <file>` entry point.
pub fn run_offload(db_path: &Path, args: &OffloadArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let content = read_input(&args.file)?;
    let namespace = args.namespace.clone().unwrap_or_else(|| "auto".to_string());
    let agent_id = resolve_agent_id(args.agent_id.as_deref())?;
    let conn = db::open(db_path).context("open db")?;
    let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
    let result = off
        .offload(&content, &namespace, args.ttl_seconds, &agent_id)
        .context("offload failed")?;
    if args.json {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&json!({
                "ref_id": result.ref_id,
                "content_sha256": result.content_sha256,
                "stored_at": result.stored_at,
                "namespace": namespace,
                "agent_id": agent_id,
            }))?
        )?;
    } else {
        writeln!(
            out.stdout,
            "offloaded {} bytes -> {} (sha256 {})",
            content.len(),
            result.ref_id,
            result.content_sha256,
        )?;
    }
    Ok(())
}

/// `ai-memory deref <ref_id>` entry point.
pub fn run_deref(db_path: &Path, args: &DerefArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let conn = db::open(db_path).context("open db")?;
    let off = ContextOffloader::new(&conn, None, OffloadConfig::default());
    // SEC-4 (Cluster D) — operator CLI is the trusted-direct-ops path
    // (see CLAUDE.md §"Agent Identity"); pass `None` to BYPASS the
    // per-agent ownership gate that the MCP handler enforces. The
    // operator can deref any blob in the local DB.
    let result = off.deref(&args.ref_id, None).context("deref failed")?;
    if let Some(path) = &args.out {
        std::fs::write(path, &result.content)
            .with_context(|| format!("write {}", path.display()))?;
    }
    if args.json {
        let body_value = if args.out.is_some() {
            Value::Null
        } else {
            Value::String(result.content)
        };
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&json!({
                "ref_id": args.ref_id,
                "sha256": result.sha256,
                "stored_at": result.stored_at,
                "bytes": body_value.as_str().map_or(0, str::len),
                "content": body_value,
            }))?
        )?;
    } else if args.out.is_none() {
        write!(out.stdout, "{}", result.content)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fresh_db_path() -> (PathBuf, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = tmp.path().join("offload-cli.db");
        (db, tmp)
    }

    #[test]
    fn run_offload_reads_file_and_round_trips() {
        let (db_path, _tmp) = fresh_db_path();
        let payload_path = _tmp.path().join("payload.txt");
        std::fs::write(&payload_path, "cli-test-body").unwrap();
        let args = OffloadArgs {
            file: payload_path.display().to_string(),
            namespace: Some("cli/test".to_string()),
            ttl_seconds: None,
            agent_id: Some("ai:cli-test".to_string()),
            json: true,
        };
        let mut buf_out = Vec::new();
        let mut buf_err = Vec::new();
        {
            let mut cli_out = CliOutput::from_std(&mut buf_out, &mut buf_err);
            run_offload(&db_path, &args, &mut cli_out).expect("offload");
        }
        let parsed: serde_json::Value = serde_json::from_slice(&buf_out).expect("json");
        let ref_id = parsed["ref_id"].as_str().expect("ref_id").to_string();
        // Deref round-trip.
        let deref_args = DerefArgs {
            ref_id: ref_id.clone(),
            out: None,
            json: false,
        };
        let mut buf_out2 = Vec::new();
        let mut buf_err2 = Vec::new();
        {
            let mut cli_out = CliOutput::from_std(&mut buf_out2, &mut buf_err2);
            run_deref(&db_path, &deref_args, &mut cli_out).expect("deref");
        }
        let body = String::from_utf8(buf_out2).unwrap();
        assert_eq!(body, "cli-test-body");
    }

    // ------------------------------------------------------------------
    // Coverage-uplift block (2026-05-19): exercise the non-JSON human-
    // render path, the --out path, the --json variant of deref, the
    // read_input file-error path, and the resolve_agent_id default
    // chain.
    // ------------------------------------------------------------------

    #[test]
    fn run_offload_human_render_emits_summary_line() {
        let (db_path, tmp) = fresh_db_path();
        let payload_path = tmp.path().join("human.txt");
        std::fs::write(&payload_path, "human-render-body").unwrap();
        let args = OffloadArgs {
            file: payload_path.display().to_string(),
            namespace: Some("cli/human".to_string()),
            ttl_seconds: None,
            agent_id: Some("ai:human-test".to_string()),
            json: false,
        };
        let mut buf_out = Vec::new();
        let mut buf_err = Vec::new();
        {
            let mut cli_out = CliOutput::from_std(&mut buf_out, &mut buf_err);
            run_offload(&db_path, &args, &mut cli_out).expect("offload");
        }
        let text = String::from_utf8(buf_out).unwrap();
        assert!(text.starts_with("offloaded "), "got: {text}");
        assert!(text.contains("bytes -> "));
        assert!(text.contains("sha256 "));
    }

    #[test]
    fn run_deref_writes_to_out_path_and_json_envelope_suppresses_content() {
        let (db_path, tmp) = fresh_db_path();
        // First offload a payload to get a ref_id.
        let payload_path = tmp.path().join("orig.txt");
        std::fs::write(&payload_path, "deref-out-body").unwrap();
        let off_args = OffloadArgs {
            file: payload_path.display().to_string(),
            namespace: Some("cli/deref-out".to_string()),
            ttl_seconds: None,
            agent_id: Some("ai:deref-out".to_string()),
            json: true,
        };
        let mut bo = Vec::new();
        let mut be = Vec::new();
        {
            let mut co = CliOutput::from_std(&mut bo, &mut be);
            run_offload(&db_path, &off_args, &mut co).expect("offload");
        }
        let parsed: serde_json::Value = serde_json::from_slice(&bo).unwrap();
        let ref_id = parsed["ref_id"].as_str().unwrap().to_string();

        // Now deref with --out and --json.
        let out_path = tmp.path().join("deref-out.bin");
        let args = DerefArgs {
            ref_id: ref_id.clone(),
            out: Some(out_path.clone()),
            json: true,
        };
        let mut bo2 = Vec::new();
        let mut be2 = Vec::new();
        {
            let mut co = CliOutput::from_std(&mut bo2, &mut be2);
            run_deref(&db_path, &args, &mut co).expect("deref");
        }
        // File written.
        let written = std::fs::read_to_string(&out_path).unwrap();
        assert_eq!(written, "deref-out-body");
        // JSON envelope present; `content` is `null` (suppressed).
        let envelope: serde_json::Value = serde_json::from_slice(&bo2).unwrap();
        assert_eq!(envelope["ref_id"], ref_id);
        assert!(envelope["content"].is_null());
        assert_eq!(envelope["bytes"].as_u64().unwrap(), 0);
    }

    #[test]
    fn run_deref_json_without_out_returns_content_inline() {
        let (db_path, tmp) = fresh_db_path();
        let payload_path = tmp.path().join("inline.txt");
        std::fs::write(&payload_path, "inline-json-body").unwrap();
        let off_args = OffloadArgs {
            file: payload_path.display().to_string(),
            namespace: Some("cli/inline".to_string()),
            ttl_seconds: None,
            agent_id: Some("ai:inline".to_string()),
            json: true,
        };
        let mut bo = Vec::new();
        let mut be = Vec::new();
        {
            let mut co = CliOutput::from_std(&mut bo, &mut be);
            run_offload(&db_path, &off_args, &mut co).expect("offload");
        }
        let parsed: serde_json::Value = serde_json::from_slice(&bo).unwrap();
        let ref_id = parsed["ref_id"].as_str().unwrap().to_string();

        let args = DerefArgs {
            ref_id: ref_id.clone(),
            out: None,
            json: true,
        };
        let mut bo2 = Vec::new();
        let mut be2 = Vec::new();
        {
            let mut co = CliOutput::from_std(&mut bo2, &mut be2);
            run_deref(&db_path, &args, &mut co).expect("deref");
        }
        let envelope: serde_json::Value = serde_json::from_slice(&bo2).unwrap();
        assert_eq!(envelope["content"].as_str().unwrap(), "inline-json-body");
        assert_eq!(
            envelope["bytes"].as_u64().unwrap(),
            "inline-json-body".len() as u64
        );
    }

    #[test]
    fn read_input_returns_error_for_missing_file() {
        let err = read_input("/nonexistent/path/never-exists.txt").unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("read "), "got: {chain}");
    }

    #[test]
    fn resolve_agent_id_uses_override_when_present() {
        let v = resolve_agent_id(Some("ai:explicit-override")).unwrap();
        assert_eq!(v, "ai:explicit-override");
    }

    #[test]
    fn resolve_agent_id_falls_back_to_default_chain_when_none() {
        // Default chain returns a non-empty stable id (host:... or
        // ai:... or anonymous:...). Pin only the non-empty property
        // so the test is hostname/env-agnostic.
        let v = resolve_agent_id(None).unwrap();
        assert!(!v.is_empty());
    }

    #[test]
    fn run_offload_propagates_read_error_for_missing_file() {
        let (db_path, _tmp) = fresh_db_path();
        let args = OffloadArgs {
            file: "/nonexistent/never-exists/file.txt".to_string(),
            namespace: None,
            ttl_seconds: None,
            agent_id: Some("ai:test".to_string()),
            json: true,
        };
        let mut bo = Vec::new();
        let mut be = Vec::new();
        let mut co = CliOutput::from_std(&mut bo, &mut be);
        let err = run_offload(&db_path, &args, &mut co).expect_err("must fail");
        let chain = format!("{err:#}");
        assert!(chain.contains("read "));
    }
}
