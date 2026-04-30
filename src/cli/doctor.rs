// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory doctor` (Phase P7 / R7) — operator-visible health dashboard.
//!
//! The doctor reads three v0.6.3.1 surfaces — Capabilities v2 (P1), data
//! integrity (P2), and recall observability (P3) — plus the v0.6.3 stats /
//! governance / subscription tables, and produces a human-readable health
//! report with severity tagging. It also has a `--json` mode for CI usage
//! and a `--remote <url>` mode that becomes the **fleet doctor** at T3+.
//!
//! Exit codes:
//!   - `0` — healthy (no warnings or critical findings).
//!   - `1` — at least one warning (and `--fail-on-warn` was passed; without
//!     the flag, warnings still keep exit 0).
//!   - `2` — at least one critical finding.
//!
//! ## Severity rules (initial)
//!
//! - **Critical:** dim_violations > 0; pending_actions older than 24h;
//!   sync skew > 600s; HNSW evictions > 0.
//! - **Warning:** silent-degrade flag from Capabilities v2
//!   (recall_mode != "hybrid" on capable tiers); subscription delivery
//!   success < 95% over the lifetime of the subscription.
//! - **Info:** anything else worth reporting.
//!
//! ## What is stubbed pending P1/P2/P3
//!
//! - **dim_violations** (P2): pre-P2 schemas have no `embedding_dim` column.
//!   `db::doctor_dim_violations` returns `Ok(None)` and the doctor renders
//!   "not yet observed (pre-P2 schema)".
//! - **HNSW evictions** (P3): the eviction counter has no SQL surface today.
//!   The doctor reports the value as 0 from a NOT_AVAILABLE-tagged section
//!   until P3 lands the in-memory counter.
//! - **recall_mode / reranker_used distribution** (P3): no rolling window
//!   has been wired yet. The doctor consults the Capabilities response
//!   for the *active* mode at this instant and reports it as the only
//!   data point.
//! - **Sync mesh** (T3+): we report `last_pulled_at` skew across
//!   `sync_state` rows when present, otherwise NOT_AVAILABLE.
//!
//! ## Anti-goals (per spec)
//!
//! - Do NOT add new monitoring infrastructure (no Prometheus, OTel exporters).
//! - Do NOT make doctor write to the DB. Read-only.
//! - Do NOT make doctor block the database. Indexed `COUNT(*)` queries only.

use crate::cli::CliOutput;
use crate::db;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

/// Severity bucket attached to every doctor finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
    /// The section couldn't be queried in this mode (e.g. raw SQL section
    /// in remote mode, or P2-dependent section on pre-P2 schema).
    NotAvailable,
}

impl Severity {
    fn label(self) -> &'static str {
        match self {
            Severity::Info => "INFO",
            Severity::Warning => "WARN",
            Severity::Critical => "CRIT",
            Severity::NotAvailable => "N/A ",
        }
    }
}

/// One section of the report. `facts` is a list of human-readable
/// `(key, value)` lines so the JSON output stays structured and the text
/// output stays scannable.
#[derive(Debug, Serialize)]
pub struct ReportSection {
    pub name: String,
    pub severity: Severity,
    pub facts: Vec<(String, String)>,
    /// Optional one-line explanation when severity != Info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The full doctor report.
#[derive(Debug, Serialize)]
pub struct Report {
    pub mode: String,
    pub source: String,
    pub generated_at: String,
    pub sections: Vec<ReportSection>,
    pub overall: Severity,
}

impl Report {
    /// Compute the overall severity as the max across sections (CRIT > WARN > INFO > N/A).
    fn rank(s: Severity) -> u8 {
        match s {
            Severity::NotAvailable => 0,
            Severity::Info => 1,
            Severity::Warning => 2,
            Severity::Critical => 3,
        }
    }

    fn compute_overall(&mut self) {
        self.overall = self
            .sections
            .iter()
            .map(|s| s.severity)
            .max_by_key(|s| Self::rank(*s))
            .unwrap_or(Severity::Info);
    }
}

/// Args from the CLI clap struct. Kept separate so `cli::doctor::run` can
/// be called directly from tests without going through clap.
pub struct DoctorArgs {
    pub remote: Option<String>,
    pub json: bool,
    pub fail_on_warn: bool,
}

/// Entry point. Returns the process exit code as a `i32` (0/1/2). The
/// caller (daemon_runtime) must `std::process::exit(code)` after the WAL
/// checkpoint has been skipped (doctor never writes).
///
/// # Errors
///
/// Returns `Err` only when the report itself cannot be written to the
/// output stream — DB / HTTP errors are folded into NOT_AVAILABLE
/// sections so a partial report still renders.
pub fn run(db_path: &Path, args: &DoctorArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    let mut report = if let Some(url) = &args.remote {
        run_remote(url, db_path)
    } else {
        run_local(db_path)
    };
    report.compute_overall();

    if args.json {
        writeln!(out.stdout, "{}", serde_json::to_string_pretty(&report)?)?;
    } else {
        render_text(&report, out)?;
    }

    let code = match report.overall {
        Severity::Critical => 2,
        Severity::Warning if args.fail_on_warn => 1,
        _ => 0,
    };
    Ok(code)
}

// ---------------------------------------------------------------------------
// Local (--db) mode
// ---------------------------------------------------------------------------

fn run_local(db_path: &Path) -> Report {
    let mut sections = Vec::with_capacity(7);

    // Open the connection once; failures bubble into a single Critical
    // section and the rest of the report is N/A.
    let conn = match db::open(db_path) {
        Ok(c) => c,
        Err(e) => {
            sections.push(ReportSection {
                name: "Storage".into(),
                severity: Severity::Critical,
                facts: vec![("error".into(), e.to_string())],
                note: Some(format!(
                    "could not open database at {} — every other section is N/A",
                    db_path.display()
                )),
            });
            return Report {
                mode: "local".into(),
                source: db_path.display().to_string(),
                generated_at: chrono::Utc::now().to_rfc3339(),
                sections,
                overall: Severity::Critical,
            };
        }
    };

    sections.push(section_storage(&conn, db_path));
    sections.push(section_index(&conn));
    sections.push(section_recall_local());
    sections.push(section_governance(&conn));
    sections.push(section_sync(&conn));
    sections.push(section_webhook(&conn));
    sections.push(section_capabilities_local());

    Report {
        mode: "local".into(),
        source: db_path.display().to_string(),
        generated_at: chrono::Utc::now().to_rfc3339(),
        sections,
        overall: Severity::Info,
    }
}

fn section_storage(conn: &rusqlite::Connection, db_path: &Path) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    match db::stats(conn, db_path) {
        Ok(stats) => {
            facts.push(("total_memories".into(), stats.total.to_string()));
            facts.push(("expiring_within_1h".into(), stats.expiring_soon.to_string()));
            facts.push(("links".into(), stats.links_count.to_string()));
            facts.push(("db_size_bytes".into(), stats.db_size_bytes.to_string()));
            for tc in &stats.by_tier {
                facts.push((format!("tier::{}", tc.tier), tc.count.to_string()));
            }
            for nc in stats.by_namespace.iter().take(10) {
                facts.push((format!("ns::{}", nc.namespace), nc.count.to_string()));
            }
        }
        Err(e) => {
            severity = Severity::Warning;
            facts.push(("stats_error".into(), e.to_string()));
        }
    }

    // dim_violations (P2 surface). Pre-P2: Ok(None) -> render N/A line, no severity bump.
    match db::doctor_dim_violations(conn) {
        Ok(Some(0)) => {
            facts.push(("dim_violations".into(), "0".into()));
        }
        Ok(Some(n)) => {
            facts.push(("dim_violations".into(), n.to_string()));
            severity = Severity::Critical;
            note = Some(format!(
                "{n} memories have an embedding dim that disagrees with their namespace's modal dim"
            ));
        }
        Ok(None) => {
            facts.push((
                "dim_violations".into(),
                "not_observed (pre-P2 schema)".into(),
            ));
        }
        Err(e) => {
            facts.push(("dim_violations_error".into(), e.to_string()));
        }
    }

    ReportSection {
        name: "Storage".into(),
        severity,
        facts,
        note,
    }
}

fn section_index(conn: &rusqlite::Connection) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    // HNSW size proxy: count of memories with an embedding (the in-memory
    // index is rebuilt from this on startup).
    let hnsw_size: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories WHERE embedding IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    facts.push(("hnsw_size_estimate".into(), hnsw_size.to_string()));

    // Cold-start cost: rough estimate of the time to rebuild HNSW on
    // daemon restart, derived from the canonical-workload measured rate
    // (~50k inserts/sec). Surfaced as a sanity-check signal, not a budget.
    let cold_start_secs = (hnsw_size as f64) / 50_000.0;
    facts.push((
        "cold_start_rebuild_secs_estimate".into(),
        format!("{cold_start_secs:.2}"),
    ));

    // Eviction counter (P3). Until P3 wires the in-memory counter into a
    // queryable surface, render NOT_AVAILABLE without a severity bump.
    facts.push((
        "index_evictions_total".into(),
        "not_observed (pre-P3 surface)".into(),
    ));

    // P3-aware path: when MAX_ENTRIES (100_000) is approached, advise the
    // operator. This is a forward-leaning hint that becomes accurate once
    // P3 lands the counter.
    if hnsw_size >= 95_000 {
        severity = Severity::Warning;
        note = Some(format!(
            "HNSW is at {hnsw_size} embeddings, within 5% of the 100k MAX_ENTRIES cap; \
             P3 will start emitting eviction events"
        ));
    }

    ReportSection {
        name: "Index".into(),
        severity,
        facts,
        note,
    }
}

fn section_recall_local() -> ReportSection {
    // Without P3's rolling window, the local doctor can only report the
    // tier configuration that *would* drive recall today. The remote
    // doctor (--remote) gets the live `recall_mode_active` from the v2
    // capabilities endpoint when P1 lands.
    ReportSection {
        name: "Recall".into(),
        severity: Severity::Info,
        facts: vec![
            (
                "recall_mode_distribution".into(),
                "not_observed (pre-P3 rolling counter)".into(),
            ),
            (
                "reranker_used_distribution".into(),
                "not_observed (pre-P3 rolling counter)".into(),
            ),
            (
                "hint".into(),
                "use --remote to read the live capabilities endpoint".into(),
            ),
        ],
        note: None,
    }
}

fn section_governance(conn: &rusqlite::Connection) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    let (with, without) = db::doctor_governance_coverage(conn).unwrap_or((0, 0));
    facts.push(("namespaces_with_policy".into(), with.to_string()));
    facts.push(("namespaces_without_policy".into(), without.to_string()));

    let dist = db::doctor_governance_depth_distribution(conn).unwrap_or_default();
    let depth_summary: String = dist
        .iter()
        .enumerate()
        .filter(|(_, n)| **n > 0)
        .map(|(d, n)| format!("d{d}={n}"))
        .collect::<Vec<_>>()
        .join(",");
    facts.push((
        "inheritance_depth".into(),
        if depth_summary.is_empty() {
            "empty".into()
        } else {
            depth_summary
        },
    ));

    match db::doctor_oldest_pending_age_secs(conn) {
        Ok(Some(age)) => {
            facts.push(("oldest_pending_age_secs".into(), age.to_string()));
            if age > 86_400 {
                severity = Severity::Critical;
                note = Some(format!(
                    "oldest pending action is {age}s old (>{} threshold = 24h)",
                    86_400
                ));
            }
        }
        Ok(None) => {
            facts.push(("oldest_pending_age_secs".into(), "queue_empty".into()));
        }
        Err(e) => {
            facts.push(("pending_query_error".into(), e.to_string()));
        }
    }

    let pending_count = db::count_pending_actions_by_status(conn, "pending").unwrap_or(0);
    facts.push(("pending_actions_total".into(), pending_count.to_string()));

    ReportSection {
        name: "Governance".into(),
        severity,
        facts,
        note,
    }
}

fn section_sync(conn: &rusqlite::Connection) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    let peer_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_state", [], |r| r.get(0))
        .unwrap_or(0);
    facts.push(("peer_count".into(), peer_count.to_string()));

    if peer_count == 0 {
        facts.push((
            "max_skew_secs".into(),
            "not_observed (no peers registered)".into(),
        ));
        return ReportSection {
            name: "Sync".into(),
            severity: Severity::NotAvailable,
            facts,
            note: Some("no sync_state rows — single-node deployment or T3+ not yet enabled".into()),
        };
    }

    match db::doctor_max_sync_skew_secs(conn) {
        Ok(Some(skew)) => {
            facts.push(("max_skew_secs".into(), skew.to_string()));
            if skew > 600 {
                severity = Severity::Critical;
                note = Some(format!(
                    "max sync skew is {skew}s (>600s threshold) — peer mesh is drifting"
                ));
            }
        }
        Ok(None) => {
            facts.push(("max_skew_secs".into(), "not_observed".into()));
        }
        Err(e) => {
            facts.push(("sync_query_error".into(), e.to_string()));
        }
    }

    ReportSection {
        name: "Sync".into(),
        severity,
        facts,
        note,
    }
}

fn section_webhook(conn: &rusqlite::Connection) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    let sub_count = db::count_subscriptions(conn).unwrap_or(0);
    facts.push(("subscription_count".into(), sub_count.to_string()));

    let (dispatched, failed) = db::doctor_webhook_delivery_totals(conn).unwrap_or((0, 0));
    facts.push(("dispatched_total".into(), dispatched.to_string()));
    facts.push(("failed_total".into(), failed.to_string()));

    if dispatched > 0 {
        let success_rate = ((dispatched.saturating_sub(failed)) as f64 / dispatched as f64) * 100.0;
        facts.push(("success_rate_pct".into(), format!("{success_rate:.2}")));
        // 95% lifetime success threshold. P5 will refine this to a
        // rolling-1h window when the dispatch table grows a timestamp
        // log; for now we use the lifetime totals already present in
        // `subscriptions.dispatch_count` / `failure_count`.
        if success_rate < 95.0 {
            severity = Severity::Warning;
            note = Some(format!(
                "lifetime delivery success {success_rate:.2}% < 95% threshold"
            ));
        }
    } else {
        facts.push(("success_rate_pct".into(), "no_deliveries_yet".into()));
    }

    ReportSection {
        name: "Webhook".into(),
        severity,
        facts,
        note,
    }
}

fn section_capabilities_local() -> ReportSection {
    // The local doctor doesn't construct a TierConfig (would require
    // loading user config). Surface the capability state via the remote
    // mode against `--remote http://localhost:9077` instead. This local
    // section just documents the gap.
    ReportSection {
        name: "Capabilities".into(),
        severity: Severity::NotAvailable,
        facts: vec![(
            "capabilities".into(),
            "use --remote <url> to query the live capabilities endpoint".into(),
        )],
        note: None,
    }
}

// ---------------------------------------------------------------------------
// Remote (--remote) mode
// ---------------------------------------------------------------------------

fn run_remote(url: &str, db_path: &Path) -> Report {
    let mut sections = Vec::with_capacity(2);

    let base = url.trim_end_matches('/');
    let cap_url = format!("{base}/api/v1/capabilities");
    let stats_url = format!("{base}/api/v1/stats");

    sections.push(section_capabilities_remote(&cap_url));
    sections.push(section_recall_remote(&cap_url));
    sections.push(section_storage_remote(&stats_url));
    sections.push(ReportSection {
        name: "Index".into(),
        severity: Severity::NotAvailable,
        facts: vec![(
            "hint".into(),
            "raw SQL section — only available in --db mode".into(),
        )],
        note: None,
    });
    sections.push(ReportSection {
        name: "Governance".into(),
        severity: Severity::NotAvailable,
        facts: vec![(
            "hint".into(),
            "raw SQL section — only available in --db mode".into(),
        )],
        note: None,
    });
    sections.push(ReportSection {
        name: "Sync".into(),
        severity: Severity::NotAvailable,
        facts: vec![(
            "hint".into(),
            "raw SQL section — only available in --db mode".into(),
        )],
        note: None,
    });
    sections.push(ReportSection {
        name: "Webhook".into(),
        severity: Severity::NotAvailable,
        facts: vec![(
            "hint".into(),
            "raw SQL section — only available in --db mode".into(),
        )],
        note: None,
    });

    Report {
        mode: "remote".into(),
        source: format!("{base} (local db reference: {})", db_path.display()),
        generated_at: chrono::Utc::now().to_rfc3339(),
        sections,
        overall: Severity::Info,
    }
}

/// Fetch a JSON document from `url` with a short timeout. Returns `Err`
/// on transport failure or non-2xx status.
fn http_get_json(url: &str) -> Result<Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("constructing HTTP client")?;
    let resp = client.get(url).send().context("HTTP GET")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("HTTP {status} from {url}");
    }
    resp.json::<Value>().context("decoding JSON response")
}

fn section_capabilities_remote(url: &str) -> ReportSection {
    let mut facts = Vec::new();
    let mut severity = Severity::Info;
    let mut note: Option<String> = None;

    match http_get_json(url) {
        Ok(v) => {
            // schema_version: "1" (legacy v0.6.3) or "2" (post-P1).
            let schema = v
                .get("schema_version")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            facts.push(("schema_version".into(), schema.to_string()));

            // P1 v2 fields — best-effort lookup. The legacy v1 shape
            // doesn't carry these; we render the missing ones as
            // "not_in_response" rather than failing.
            let recall_mode = v
                .get("features")
                .and_then(|f| f.get("recall_mode_active"))
                .and_then(Value::as_str)
                .unwrap_or("not_in_response");
            facts.push(("recall_mode_active".into(), recall_mode.to_string()));

            let reranker = v
                .get("features")
                .and_then(|f| f.get("reranker_active"))
                .and_then(Value::as_str)
                .unwrap_or("not_in_response");
            facts.push(("reranker_active".into(), reranker.to_string()));

            // Severity hints. recall_mode in {"degraded", "disabled",
            // "keyword_only"} bumps to Warning when the tier is supposed
            // to support hybrid (semantic / smart / autonomous).
            if matches!(recall_mode, "degraded" | "disabled" | "keyword_only") {
                let tier = v.get("feature_tier").and_then(Value::as_str).unwrap_or("");
                if matches!(tier, "semantic" | "smart" | "autonomous") {
                    severity = Severity::Warning;
                    note = Some(format!(
                        "tier={tier} but recall_mode_active={recall_mode} — silent degradation"
                    ));
                }
            }
        }
        Err(e) => {
            severity = Severity::Critical;
            facts.push(("error".into(), e.to_string()));
            note = Some(format!("could not reach {url}"));
        }
    }

    ReportSection {
        name: "Capabilities".into(),
        severity,
        facts,
        note,
    }
}

fn section_recall_remote(cap_url: &str) -> ReportSection {
    let mut facts = Vec::new();
    let severity = Severity::Info;

    if let Ok(v) = http_get_json(cap_url) {
        let recall_mode = v
            .get("features")
            .and_then(|f| f.get("recall_mode_active"))
            .and_then(Value::as_str)
            .unwrap_or("not_in_response");
        facts.push(("active_recall_mode".into(), recall_mode.to_string()));
        let reranker = v
            .get("features")
            .and_then(|f| f.get("reranker_active"))
            .and_then(Value::as_str)
            .unwrap_or("not_in_response");
        facts.push(("active_reranker".into(), reranker.to_string()));
        facts.push((
            "recall_mode_distribution".into(),
            "not_observed (pre-P3 rolling counter)".into(),
        ));
    } else {
        facts.push(("error".into(), "could not fetch capabilities".into()));
    }

    ReportSection {
        name: "Recall".into(),
        severity,
        facts,
        note: None,
    }
}

fn section_storage_remote(stats_url: &str) -> ReportSection {
    let mut facts = Vec::new();
    let severity = Severity::Info;

    match http_get_json(stats_url) {
        Ok(v) => {
            if let Some(total) = v.get("total").and_then(Value::as_u64) {
                facts.push(("total_memories".into(), total.to_string()));
            }
            if let Some(exp) = v.get("expiring_soon").and_then(Value::as_u64) {
                facts.push(("expiring_within_1h".into(), exp.to_string()));
            }
            if let Some(links) = v.get("links_count").and_then(Value::as_u64) {
                facts.push(("links".into(), links.to_string()));
            }
            facts.push((
                "dim_violations".into(),
                "not_in_remote_response (P2 surface lands at /api/v1/stats)".into(),
            ));
        }
        Err(e) => {
            facts.push(("error".into(), e.to_string()));
        }
    }

    ReportSection {
        name: "Storage".into(),
        severity,
        facts,
        note: None,
    }
}

// ---------------------------------------------------------------------------
// Text rendering
// ---------------------------------------------------------------------------

fn render_text(report: &Report, out: &mut CliOutput<'_>) -> Result<()> {
    writeln!(out.stdout, "ai-memory doctor — {} mode", report.mode)?;
    writeln!(out.stdout, "  source:       {}", report.source)?;
    writeln!(out.stdout, "  generated_at: {}", report.generated_at)?;
    writeln!(out.stdout, "  overall:      {}", report.overall.label())?;
    writeln!(out.stdout)?;
    for section in &report.sections {
        writeln!(
            out.stdout,
            "[{}] {}",
            section.severity.label(),
            section.name
        )?;
        for (k, v) in &section.facts {
            writeln!(out.stdout, "    {k:<32} {v}")?;
        }
        if let Some(note) = &section.note {
            writeln!(out.stdout, "    note: {note}")?;
        }
        writeln!(out.stdout)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests (unit-level — full integration tests live in tests/doctor_cli.rs)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_rank_orders_critical_highest() {
        assert!(Report::rank(Severity::Critical) > Report::rank(Severity::Warning));
        assert!(Report::rank(Severity::Warning) > Report::rank(Severity::Info));
        assert!(Report::rank(Severity::Info) > Report::rank(Severity::NotAvailable));
    }

    #[test]
    fn compute_overall_picks_critical_when_present() {
        let mut r = Report {
            mode: "local".into(),
            source: ":memory:".into(),
            generated_at: "now".into(),
            sections: vec![
                ReportSection {
                    name: "A".into(),
                    severity: Severity::Info,
                    facts: vec![],
                    note: None,
                },
                ReportSection {
                    name: "B".into(),
                    severity: Severity::Critical,
                    facts: vec![],
                    note: None,
                },
                ReportSection {
                    name: "C".into(),
                    severity: Severity::Warning,
                    facts: vec![],
                    note: None,
                },
            ],
            overall: Severity::Info,
        };
        r.compute_overall();
        assert_eq!(r.overall, Severity::Critical);
    }

    #[test]
    fn compute_overall_picks_warning_when_no_critical() {
        let mut r = Report {
            mode: "local".into(),
            source: ":memory:".into(),
            generated_at: "now".into(),
            sections: vec![
                ReportSection {
                    name: "A".into(),
                    severity: Severity::Info,
                    facts: vec![],
                    note: None,
                },
                ReportSection {
                    name: "B".into(),
                    severity: Severity::Warning,
                    facts: vec![],
                    note: None,
                },
            ],
            overall: Severity::Info,
        };
        r.compute_overall();
        assert_eq!(r.overall, Severity::Warning);
    }
}
