// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory verify-signed-events-chain` — walk the SQL-side
//! `signed_events` cross-row hash chain (v34, #698 V-4 closeout) and
//! emit a structured chain-integrity report.
//!
//! Distinct from `verify-reflection-chain` (which walks the
//! reflects_on edges in `memory_links`) and from `audit verify`
//! (which walks the JSONL audit log under `<audit_dir>/audit.log`).
//! Three complementary verifiers, three load-bearing properties:
//!
//! - `verify-signed-events-chain` (this surface): the SQL-side
//!   cross-row hash chain on `signed_events`. Daemon-local
//!   tamper-evidence; auditor reads it directly from the database.
//! - `audit verify`: the on-disk JSONL chain. Portable evidence
//!   format for handoff to a SIEM.
//! - `verify-reflection-chain`: per-edge Ed25519 signatures on
//!   `reflects_on` links. Reflection ancestry attestation.
//!
//! ## Exit codes
//!
//! - `0` — chain fully verified.
//! - `1` — chain break detected (sequence gap, duplicate, or
//!   `prev_hash` mismatch).
//!
//! ## Output formats
//!
//! - `--format text` (default) — one-line human report on stdout.
//! - `--format json` — machine-parseable report mirroring the
//!   [`crate::signed_events::ChainVerificationReport`] shape.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::cli::CliOutput;

/// Arguments for `ai-memory verify-signed-events-chain`.
#[derive(clap::Args, Debug)]
pub struct VerifySignedEventsChainArgs {
    /// Lower-bound sequence (exclusive). Rows with
    /// `sequence > since` are walked; rows at or below `since` are
    /// trusted as previously-verified. Default 0 (walk every row).
    #[arg(long, value_name = "SEQUENCE", default_value_t = 0)]
    pub since: i64,

    /// Output format: `text` (default) or `json`.
    #[arg(long, value_name = "FORMAT", default_value = "text")]
    pub format: String,
}

/// JSON-serialised mirror of
/// [`crate::signed_events::ChainVerificationReport`]. We don't
/// derive `Serialize` on the original because it lives in a
/// non-CLI module; the CLI layer owns the wire shape.
#[derive(Debug, Serialize)]
pub struct ChainVerifyReportJson {
    pub rows_checked: u64,
    pub chain_break: Option<i64>,
    pub signature_failures: Vec<i64>,
    pub chain_holds: bool,
}

/// Run the verifier. Returns the desired process exit code (0 on
/// chain GREEN, 1 on chain break).
///
/// # Errors
///
/// Returns the underlying `rusqlite` or formatter error if the SQL
/// query or the report rendering fails.
pub fn run(
    db_path: &Path,
    args: &VerifySignedEventsChainArgs,
    out: &mut CliOutput<'_>,
) -> Result<i32> {
    let conn =
        crate::db::open(db_path).with_context(|| format!("open db at {}", db_path.display()))?;
    let since = if args.since > 0 {
        Some(args.since)
    } else {
        None
    };
    let report = crate::signed_events::verify_chain(&conn, since)
        .context("verify_chain over signed_events")?;
    let holds = report.chain_holds();

    match args.format.as_str() {
        "json" => {
            let wire = ChainVerifyReportJson {
                rows_checked: report.rows_checked,
                chain_break: report.chain_break,
                signature_failures: report.signature_failures.clone(),
                chain_holds: holds,
            };
            let json = serde_json::to_string_pretty(&wire).context("serialize chain report")?;
            writeln!(out.stdout, "{json}").context("write chain report")?;
        }
        _ => {
            // text — one-line summary on stdout.
            if holds {
                writeln!(
                    out.stdout,
                    "verify-signed-events-chain OK: {} row(s) walked, chain holds",
                    report.rows_checked,
                )
                .context("write chain report")?;
            } else {
                let where_ = report
                    .chain_break
                    .map_or_else(|| "<unknown>".to_string(), |s| s.to_string());
                writeln!(
                    out.stdout,
                    "verify-signed-events-chain FAIL: chain break at sequence={where_} \
                     ({} row(s) walked)",
                    report.rows_checked,
                )
                .context("write chain report")?;
            }
        }
    }

    Ok(if holds { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signed_events::{SignedEvent, append_signed_event, payload_hash};

    fn fixture_event(payload: &[u8]) -> SignedEvent {
        SignedEvent {
            id: uuid::Uuid::new_v4().to_string(),
            agent_id: "alice".to_string(),
            event_type: "memory_link.created".to_string(),
            payload_hash: payload_hash(payload),
            signature: None,
            attest_level: "unsigned".to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..SignedEvent::default()
        }
    }

    fn temp_db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("verify-signed-events-")
            .tempdir()
            .expect("tempdir");
        let path = dir.path().join("test.db");
        drop(crate::db::open(&path).expect("init db"));
        (dir, path)
    }

    #[test]
    fn empty_db_reports_zero_rows_chain_holds() {
        let (_dir, path) = temp_db();
        let args = VerifySignedEventsChainArgs {
            since: 0,
            format: "json".to_string(),
        };
        let mut buf_out = Vec::<u8>::new();
        let mut buf_err = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut buf_out, &mut buf_err);
        let code = run(&path, &args, &mut out).expect("run");
        assert_eq!(code, 0, "empty chain holds vacuously");
        let s = String::from_utf8(buf_out).expect("utf-8");
        assert!(s.contains("\"chain_holds\": true"), "got: {s}");
        assert!(s.contains("\"rows_checked\": 0"), "got: {s}");
    }

    #[test]
    fn populated_db_reports_chain_ok() {
        let (_dir, path) = temp_db();
        {
            let conn = crate::db::open(&path).expect("open");
            for i in 0..3 {
                append_signed_event(&conn, &fixture_event(format!("payload-{i}").as_bytes()))
                    .expect("append");
            }
        }
        let args = VerifySignedEventsChainArgs {
            since: 0,
            format: "text".to_string(),
        };
        let mut buf_out = Vec::<u8>::new();
        let mut buf_err = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut buf_out, &mut buf_err);
        let code = run(&path, &args, &mut out).expect("run");
        assert_eq!(code, 0, "3-row clean chain holds; got code={code}");
        let s = String::from_utf8(buf_out).expect("utf-8");
        assert!(s.contains("OK"), "got: {s}");
        assert!(s.contains("3 row(s) walked"), "got: {s}");
    }

    // Note: The tampered-chain → exit-code-1 path is covered by the
    // integration test `tests/signed_events_chain_v34.rs::
    // tamper_in_middle_row_breaks_chain` (calling `verify_chain`
    // directly) and is intentionally NOT duplicated here — exercising
    // `UPDATE signed_events` from a `src/` file (even under `#[cfg(test)]`)
    // would trip the `append_only_invariant_no_mutators_in_src`
    // guard in `signed_events.rs`.
}
