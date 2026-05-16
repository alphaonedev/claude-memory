// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! v0.7.0 Form 5 (issue #758) — `ai-memory calibrate confidence
//! --from-shadow` CLI subcommand.
//!
//! Reads `confidence_shadow_observations` from the last `--days N`
//! days and emits a per-(namespace, source) baseline report. Two output
//! formats:
//!
//!   * `--output-format json` (default): structured JSON envelope of
//!     [`crate::confidence::calibrate::CalibrationReport`].
//!   * `--output-format table`: a human-readable ASCII table with
//!     `(namespace, source, count, median, mean, bucket-histogram)`
//!     columns for quick operator review.
//!
//! Audit-honest contract: the sweep is **read-only**. Operators review
//! the report before deciding whether to persist baselines into a
//! calibration store (operator-driven in a follow-up; v0.7.0 ships the
//! observation pipeline + report only).

use std::path::Path;

use anyhow::Result;
use clap::{Args, ValueEnum};
use rusqlite::Connection;

use crate::cli::CliOutput;
use crate::confidence::calibrate::{CalibrationReport, DEFAULT_WINDOW_DAYS, calibrate_from_shadow};

/// Output format for the calibration report.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum OutputFormat {
    /// Structured JSON envelope ([`CalibrationReport`]) — default.
    #[default]
    Json,
    /// Human-readable ASCII table.
    Table,
}

/// Top-level CLI args for `ai-memory calibrate <subcommand>`.
///
/// Only `confidence` is wired today; the verb stays open for future
/// calibration surfaces (e.g., recall blend weights) without re-pinning
/// the public CLI surface.
#[derive(Args, Debug, Clone)]
pub struct CalibrateArgs {
    #[command(subcommand)]
    pub subcommand: CalibrateSubcommand,
}

/// Subcommand discriminator.
#[derive(clap::Subcommand, Debug, Clone)]
pub enum CalibrateSubcommand {
    /// Scan `confidence_shadow_observations` and emit per-(namespace,
    /// source) baselines.
    Confidence(CalibrateConfidenceArgs),
}

/// CLI args for `ai-memory calibrate confidence --from-shadow`.
#[derive(Args, Debug, Clone)]
pub struct CalibrateConfidenceArgs {
    /// Read shadow observations rather than caller-confidence rows.
    /// Required in v0.7.0 (the only mode the sweep ships with); reserved
    /// for future modes like `--from-recall-traces`.
    #[arg(long, default_value_t = true)]
    pub from_shadow: bool,

    /// Window size in days. Defaults to 30
    /// ([`crate::confidence::calibrate::DEFAULT_WINDOW_DAYS`]).
    #[arg(long, default_value_t = DEFAULT_WINDOW_DAYS)]
    pub days: i64,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub output_format: OutputFormat,
}

/// Dispatch entry-point. Called from `daemon_runtime::run`.
///
/// Returns `Ok(0)` on success and a non-zero exit code on a validated
/// failure mode (DB unavailable, sweep error).
///
/// # Errors
///
/// Propagates DB and serialisation errors. The shadow observation
/// table is created by the v39 migration; running the sweep against a
/// pre-v39 DB surfaces the SQL error from the substrate.
pub fn run(db_path: &Path, args: &CalibrateConfidenceArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    if !args.from_shadow {
        writeln!(
            out.stderr,
            "calibrate confidence: --from-shadow is the only supported mode in v0.7.0; \
             pass --from-shadow to scan the observation table."
        )?;
        return Ok(2);
    }

    let conn = Connection::open(db_path)?;
    let report = calibrate_from_shadow(&conn, args.days, chrono::Utc::now())?;

    let buf = match args.output_format {
        OutputFormat::Json => serde_json::to_string_pretty(&report)?,
        OutputFormat::Table => render_table(&report),
    };
    writeln!(out.stdout, "{buf}")?;
    Ok(0)
}

/// Render the report as a fixed-width ASCII table. Format:
///
/// ```text
/// CONFIDENCE CALIBRATION REPORT (window: 30 days, observations: 42)
///
/// NAMESPACE         SOURCE       COUNT  MEDIAN  MEAN   HISTOGRAM (0.0..1.0)
/// ai-memory-mcp     user         12     0.62    0.61   ..#.##.#.##
/// ai-memory-mcp     claude       8      0.74    0.73   ...#####.#.
/// ```
fn render_table(report: &CalibrationReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "CONFIDENCE CALIBRATION REPORT (window: {} days, observations: {})\n\n",
        report.window_days, report.total_observations
    ));
    out.push_str(&format!(
        "{:<24}  {:<12}  {:>6}  {:>6}  {:>6}  HISTOGRAM (0.0..1.0)\n",
        "NAMESPACE", "SOURCE", "COUNT", "MEDIAN", "MEAN"
    ));
    if report.baselines.is_empty() {
        out.push_str("(no observations in window)\n");
        return out;
    }
    for b in &report.baselines {
        let hist: String = b
            .buckets
            .iter()
            .map(|c| if *c == 0 { '.' } else { '#' })
            .collect();
        out.push_str(&format!(
            "{:<24}  {:<12}  {:>6}  {:>6.2}  {:>6.2}  {hist}\n",
            b.namespace, b.source, b.count, b.median, b.mean,
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_report() -> CalibrationReport {
        CalibrationReport {
            window_days: 30,
            total_observations: 0,
            baselines: Vec::new(),
        }
    }

    #[test]
    fn render_table_handles_empty() {
        let s = render_table(&empty_report());
        assert!(s.contains("window: 30 days"));
        assert!(s.contains("no observations in window"));
    }

    #[test]
    fn render_table_emits_one_row_per_baseline() {
        let r = CalibrationReport {
            window_days: 7,
            total_observations: 3,
            baselines: vec![crate::confidence::calibrate::PerSourceBaseline {
                namespace: "ns".to_string(),
                source: "user".to_string(),
                count: 3,
                median: 0.5,
                mean: 0.55,
                buckets: [0, 0, 1, 0, 1, 1, 0, 0, 0, 0],
            }],
        };
        let s = render_table(&r);
        assert!(s.contains("ns"));
        assert!(s.contains("user"));
        assert!(s.contains("0.50"));
    }
}
