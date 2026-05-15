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
    let result = off.deref(&args.ref_id).context("deref failed")?;
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
}
