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

/// v0.6.4-004 — Args for `ai-memory doctor --tokens`. Routes to
/// [`run_tokens`] instead of the regular health pass.
#[derive(Debug, Default)]
pub struct TokensArgs {
    /// Emit structured JSON instead of human-readable.
    pub json: bool,
    /// Dump the full per-tool size table (implies `json`).
    pub raw_table: bool,
    /// Hypothetical profile to evaluate (defaults to `core` —
    /// the v0.6.4 default).
    pub profile: Option<String>,
    /// v0.7-G3 — also append the hook-executor metrics block.
    /// Operators running `--tokens --hooks` see both surfaces in
    /// one pass.
    pub hooks: bool,
}

/// v0.7-G3 — Args for `ai-memory doctor --hooks` (standalone).
/// Routes to [`run_hooks`].
#[derive(Debug, Default)]
pub struct HooksReportArgs {
    /// Emit structured JSON instead of human-readable.
    pub json: bool,
}

/// v0.6.4-004 — token-cost report.
///
/// Walks `crate::sizes::tool_sizes()`, groups by family via
/// `crate::profile::Family::for_tool`, rolls up per-profile totals,
/// and emits either a human-readable table or a JSON document.
///
/// Returns 0 on success. Errors when the `--profile` flag is malformed
/// (the doctor's job is to surface the same diagnostic the MCP server
/// would, not to crash with a stack trace) — those exit code 2.
pub fn run_tokens(args: TokensArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    use crate::profile::{Family, Profile};
    use crate::sizes;

    // Resolve the hypothetical profile. Default to `core` since that
    // is what v0.6.4 ships and what the operator wants to see savings
    // *against*.
    let profile = match Profile::parse(args.profile.as_deref().unwrap_or("core")) {
        Ok(p) => p,
        Err(e) => {
            writeln!(out.stderr, "ai-memory doctor --tokens: {e}")?;
            return Ok(2);
        }
    };

    let table = sizes::tool_sizes();
    let full_total: usize = table.iter().map(|t| t.total_tokens).sum();
    let active_total: usize = table
        .iter()
        .filter(|t| profile.loads(&t.name))
        .map(|t| t.total_tokens)
        .sum();
    let savings = full_total.saturating_sub(active_total);
    let pct = if full_total == 0 {
        0.0
    } else {
        (f64::from(u32::try_from(savings).unwrap_or(u32::MAX))
            / f64::from(u32::try_from(full_total).unwrap_or(u32::MAX)))
            * 100.0
    };

    // Per-family rollup. Includes "always-on" pseudo bucket for tools
    // that load regardless of profile (today: just memory_capabilities).
    let mut family_totals: Vec<(String, usize, usize)> = Family::all()
        .iter()
        .map(|f| {
            let mut tool_count = 0usize;
            let mut sum = 0usize;
            for entry in table {
                if Family::for_tool(&entry.name) == Some(*f) {
                    tool_count += 1;
                    sum += entry.total_tokens;
                }
            }
            (f.name().to_string(), tool_count, sum)
        })
        .collect();
    family_totals.sort_by_key(|(_, _, sum)| std::cmp::Reverse(*sum));

    if args.json || args.raw_table {
        // Always include the full per-tool table when --raw-table is
        // set; --json gives the rolled-up view.
        let payload = serde_json::json!({
            "schema_version": "v0.6.4-tokens-1",
            "tokenizer": "cl100k_base",
            "active_profile": profile.families().iter().map(|f| f.name()).collect::<Vec<_>>(),
            "active_total_tokens": active_total,
            "full_profile_total_tokens": full_total,
            "savings_tokens": savings,
            "savings_pct": format!("{pct:.1}"),
            "families": family_totals.iter().map(|(name, count, sum)| {
                // Resolve family enum from the name to ask whether
                // it is loaded under the active profile.
                let fam = Family::all()
                    .iter()
                    .find(|f| f.name() == name)
                    .copied()
                    .unwrap_or(Family::Other);
                serde_json::json!({
                    "name": name,
                    "tool_count": count,
                    "tokens": sum,
                    "loaded": profile.includes(fam),
                })
            }).collect::<Vec<_>>(),
            "tools": if args.raw_table {
                serde_json::Value::Array(
                    table.iter().map(|t| serde_json::json!({
                        "name": t.name,
                        "tokens": t.total_tokens,
                        "family": Family::for_tool(&t.name).map(|f| f.name()),
                        "loaded_under_active_profile": profile.loads(&t.name),
                    })).collect()
                )
            } else {
                serde_json::Value::Null
            },
        });
        writeln!(out.stdout, "{}", serde_json::to_string_pretty(&payload)?)?;
        return Ok(0);
    }

    // Human-readable.
    writeln!(out.stdout, "ai-memory doctor --tokens")?;
    writeln!(
        out.stdout,
        "  Tokenizer: cl100k_base (Claude / GPT input accounting)"
    )?;
    writeln!(
        out.stdout,
        "  Active profile: {}",
        profile
            .families()
            .iter()
            .map(|f| f.name())
            .collect::<Vec<_>>()
            .join(",")
    )?;
    writeln!(out.stdout)?;
    writeln!(out.stdout, "  Tool surface cost:")?;
    writeln!(
        out.stdout,
        "    Active ({:>2} tools loaded): {:>6} tokens",
        table.iter().filter(|t| profile.loads(&t.name)).count(),
        active_total
    )?;
    writeln!(
        out.stdout,
        "    Full   ({:>2} tools loaded): {:>6} tokens",
        table.len(),
        full_total
    )?;
    writeln!(
        out.stdout,
        "    Savings vs full:           {:>6} tokens ({pct:.1}%)",
        savings
    )?;
    writeln!(out.stdout)?;
    writeln!(out.stdout, "  Per-family breakdown (sorted by total cost):")?;
    for (name, count, sum) in &family_totals {
        writeln!(
            out.stdout,
            "    {name:<12} {count:>2} tools  {sum:>6} tokens",
        )?;
    }
    if args.hooks {
        writeln!(out.stdout)?;
        render_hooks_human(out)?;
    }
    Ok(0)
}

/// v0.7-G3 — `ai-memory doctor --hooks` entry point. Renders the
/// loaded `hooks.toml` shape plus zeroed metric placeholders.
///
/// The CLI process is *not* the running daemon — it can't reach the
/// in-process `ExecutorRegistry`. Until G7-G11 wires the executor
/// into the actual memory operation points, this surface reports
/// the loaded config + a zeroed metrics row per hook so operators
/// can sanity-check their `hooks.toml` (and so the doctor JSON
/// schema stabilizes for the dashboard work that lands alongside).
pub fn run_hooks(args: HooksReportArgs, out: &mut CliOutput<'_>) -> Result<i32> {
    use crate::hooks::config::HookConfig;

    let path_opt = HookConfig::default_path();
    let hooks: Vec<HookConfig> = match path_opt.as_ref() {
        Some(p) if p.exists() => match HookConfig::load_from_file(p) {
            Ok(h) => h,
            Err(e) => {
                writeln!(out.stderr, "ai-memory doctor --hooks: {e}")?;
                return Ok(2);
            }
        },
        _ => Vec::new(),
    };

    if args.json {
        let payload = serde_json::json!({
            "schema_version": "v0.7-hooks-1",
            "config_path": path_opt.as_ref().map(|p| p.display().to_string()),
            "hooks_loaded": hooks.len(),
            "executors": hooks.iter().map(|h| serde_json::json!({
                "event": h.event,
                "command": h.command.display().to_string(),
                "mode": h.mode,
                "namespace": h.namespace,
                "priority": h.priority,
                "timeout_ms": h.timeout_ms,
                "enabled": h.enabled,
                "metrics": {
                    "events_fired": 0,
                    "events_dropped": 0,
                    "mean_latency_us": 0,
                },
            })).collect::<Vec<_>>(),
            "note": "metrics placeholders until G7-G11 wires the executor into the daemon",
        });
        writeln!(out.stdout, "{}", serde_json::to_string_pretty(&payload)?)?;
        return Ok(0);
    }

    render_hooks_human_with(out, path_opt.as_deref(), &hooks)?;
    Ok(0)
}

/// Human-readable hooks block. Used by `--hooks` standalone *and*
/// by the appended block when the operator combines `--tokens --hooks`.
fn render_hooks_human(out: &mut CliOutput<'_>) -> Result<()> {
    use crate::hooks::config::HookConfig;
    let path_opt = HookConfig::default_path();
    let hooks: Vec<HookConfig> = match path_opt.as_ref() {
        Some(p) if p.exists() => HookConfig::load_from_file(p).unwrap_or_default(),
        _ => Vec::new(),
    };
    render_hooks_human_with(out, path_opt.as_deref(), &hooks)
}

fn render_hooks_human_with(
    out: &mut CliOutput<'_>,
    path: Option<&Path>,
    hooks: &[crate::hooks::config::HookConfig],
) -> Result<()> {
    writeln!(out.stdout, "ai-memory doctor --hooks")?;
    if let Some(p) = path {
        writeln!(out.stdout, "  Config path: {}", p.display())?;
    }
    writeln!(out.stdout, "  Hooks loaded: {}", hooks.len())?;
    if hooks.is_empty() {
        writeln!(
            out.stdout,
            "  (no hooks configured — drop a hooks.toml at the path above to enable)"
        )?;
        return Ok(());
    }
    writeln!(out.stdout)?;
    writeln!(
        out.stdout,
        "  {:<26} {:<8} {:<22} fired dropped mean_us",
        "event", "mode", "command"
    )?;
    for h in hooks {
        let event = format!("{:?}", h.event);
        let mode = format!("{:?}", h.mode);
        let cmd = h
            .command
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| h.command.display().to_string());
        let cmd_truncated: String = cmd.chars().take(22).collect();
        writeln!(
            out.stdout,
            "  {event:<26} {mode:<8} {cmd_truncated:<22} {:>5} {:>7} {:>7}",
            0, 0, 0,
        )?;
    }
    writeln!(out.stdout)?;
    writeln!(
        out.stdout,
        "  note: live metrics land when G7-G11 wires the executor into the daemon."
    )?;
    Ok(())
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

    // v0.7.0 K3 — surface the active permissions.mode + per-mode
    // decision counts so operators can verify the gate is wired and
    // observe drift between advertised and enforced policy.
    let mode = crate::config::active_permissions_mode();
    facts.push(("permissions_mode".into(), mode.as_str().to_string()));
    let counts = crate::config::permissions_decision_counts();
    facts.push(("decisions::enforce".into(), counts.enforce.to_string()));
    facts.push(("decisions::advisory".into(), counts.advisory.to_string()));
    facts.push(("decisions::off".into(), counts.off.to_string()));

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
#[allow(clippy::too_many_lines, clippy::similar_names)]
mod tests {
    use super::*;
    use crate::cli::CliOutput;
    use crate::cli::test_utils::{TestEnv, seed_memory};
    use rusqlite::params;

    // -------------------------------------------------------------------
    // Severity / Report helpers (pure, no DB)
    // -------------------------------------------------------------------

    #[test]
    fn severity_rank_orders_critical_highest() {
        assert!(Report::rank(Severity::Critical) > Report::rank(Severity::Warning));
        assert!(Report::rank(Severity::Warning) > Report::rank(Severity::Info));
        assert!(Report::rank(Severity::Info) > Report::rank(Severity::NotAvailable));
    }

    #[test]
    fn severity_label_renders_for_every_variant() {
        assert_eq!(Severity::Info.label(), "INFO");
        assert_eq!(Severity::Warning.label(), "WARN");
        assert_eq!(Severity::Critical.label(), "CRIT");
        assert_eq!(Severity::NotAvailable.label(), "N/A ");
    }

    #[test]
    fn severity_serializes_lowercase_and_round_trips() {
        // The Serialize derive uses `rename_all = "lowercase"`. We don't
        // derive Deserialize, so we round-trip via the JSON Value form.
        let s = serde_json::to_value(Severity::Critical).unwrap();
        assert_eq!(s, serde_json::Value::String("critical".into()));
        let s = serde_json::to_value(Severity::NotAvailable).unwrap();
        assert_eq!(s, serde_json::Value::String("notavailable".into()));
    }

    fn mk_section(name: &str, severity: Severity) -> ReportSection {
        ReportSection {
            name: name.into(),
            severity,
            facts: vec![("k".into(), "v".into())],
            note: None,
        }
    }

    fn mk_report(sections: Vec<ReportSection>) -> Report {
        Report {
            mode: "local".into(),
            source: ":memory:".into(),
            generated_at: "now".into(),
            sections,
            overall: Severity::Info,
        }
    }

    #[test]
    fn compute_overall_picks_critical_when_present() {
        let mut r = mk_report(vec![
            mk_section("A", Severity::Info),
            mk_section("B", Severity::Critical),
            mk_section("C", Severity::Warning),
        ]);
        r.compute_overall();
        assert_eq!(r.overall, Severity::Critical);
    }

    #[test]
    fn compute_overall_picks_warning_when_no_critical() {
        let mut r = mk_report(vec![
            mk_section("A", Severity::Info),
            mk_section("B", Severity::Warning),
        ]);
        r.compute_overall();
        assert_eq!(r.overall, Severity::Warning);
    }

    #[test]
    fn compute_overall_picks_info_when_no_warnings_or_critical() {
        let mut r = mk_report(vec![
            mk_section("A", Severity::NotAvailable),
            mk_section("B", Severity::Info),
        ]);
        r.compute_overall();
        assert_eq!(r.overall, Severity::Info);
    }

    #[test]
    fn compute_overall_handles_empty_sections() {
        let mut r = mk_report(vec![]);
        r.compute_overall();
        // unwrap_or fallback path — empty iterator collapses to Info.
        assert_eq!(r.overall, Severity::Info);
    }

    #[test]
    fn compute_overall_only_n_a_yields_n_a() {
        let mut r = mk_report(vec![
            mk_section("A", Severity::NotAvailable),
            mk_section("B", Severity::NotAvailable),
        ]);
        r.compute_overall();
        assert_eq!(r.overall, Severity::NotAvailable);
    }

    // -------------------------------------------------------------------
    // ReportSection / Report serde shape
    // -------------------------------------------------------------------

    #[test]
    fn report_section_serializes_with_expected_keys() {
        let section = ReportSection {
            name: "Storage".into(),
            severity: Severity::Warning,
            facts: vec![("total".into(), "5".into())],
            note: Some("hello".into()),
        };
        let v = serde_json::to_value(&section).unwrap();
        assert_eq!(v["name"], "Storage");
        assert_eq!(v["severity"], "warning");
        // Facts is a list of 2-tuples encoded as JSON arrays.
        assert!(v["facts"].is_array());
        assert_eq!(v["facts"][0][0], "total");
        assert_eq!(v["facts"][0][1], "5");
        assert_eq!(v["note"], "hello");
    }

    #[test]
    fn report_section_skips_note_when_none() {
        let section = ReportSection {
            name: "Recall".into(),
            severity: Severity::Info,
            facts: vec![],
            note: None,
        };
        let v = serde_json::to_value(&section).unwrap();
        assert!(
            v.get("note").is_none(),
            "note=None must be skipped per #[serde(skip_serializing_if)]"
        );
    }

    #[test]
    fn report_top_level_serialization_has_all_fields() {
        let r = mk_report(vec![mk_section("S", Severity::Info)]);
        let v = serde_json::to_value(&r).unwrap();
        for k in ["mode", "source", "generated_at", "sections", "overall"] {
            assert!(v.get(k).is_some(), "expected key {k} in JSON");
        }
        assert_eq!(v["sections"].as_array().unwrap().len(), 1);
    }

    // -------------------------------------------------------------------
    // Local-DB mode — basic happy path
    // -------------------------------------------------------------------

    fn run_local_collect(db_path: &Path) -> Report {
        let mut report = run_local(db_path);
        report.compute_overall();
        report
    }

    fn find<'a>(report: &'a Report, name: &str) -> &'a ReportSection {
        report
            .sections
            .iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("section {name} not found"))
    }

    fn fact<'a>(section: &'a ReportSection, key: &str) -> &'a str {
        section
            .facts
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("fact {key} not found in section {}", section.name))
    }

    #[test]
    fn local_run_on_empty_db_produces_seven_sections() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        assert_eq!(report.mode, "local");
        assert_eq!(report.sections.len(), 7);
        let names: Vec<&str> = report.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Storage",
                "Index",
                "Recall",
                "Governance",
                "Sync",
                "Webhook",
                "Capabilities"
            ]
        );
    }

    #[test]
    fn local_run_empty_db_storage_section_is_info() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let storage = find(&report, "Storage");
        assert_eq!(storage.severity, Severity::Info);
        assert_eq!(fact(storage, "total_memories"), "0");
        // Pre-P2 schema (current release) has no `embedding_dim` column —
        // `db::doctor_dim_violations` returns Ok(None), rendered as
        // "not_observed (pre-P2 schema)".
        let dim = fact(storage, "dim_violations");
        assert!(
            dim.contains("not_observed") || dim == "0",
            "unexpected dim_violations value: {dim}"
        );
    }

    #[test]
    fn local_run_with_seeded_memory_reports_total() {
        let env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns-a", "title-1", "content one");
        seed_memory(&env.db_path, "ns-a", "title-2", "content two");
        seed_memory(&env.db_path, "ns-b", "title-3", "content three");
        let report = run_local_collect(&env.db_path);
        let storage = find(&report, "Storage");
        assert_eq!(fact(storage, "total_memories"), "3");
        // Tier breakdown — seed_memory inserts at tier=mid.
        let tier_mid = storage
            .facts
            .iter()
            .find(|(k, _)| k == "tier::mid")
            .map(|(_, v)| v.as_str());
        assert_eq!(tier_mid, Some("3"));
        // Namespace breakdown caps at 10 entries; 2 namespaces fit.
        let ns_a = storage
            .facts
            .iter()
            .find(|(k, _)| k == "ns::ns-a")
            .map(|(_, v)| v.as_str());
        let ns_b = storage
            .facts
            .iter()
            .find(|(k, _)| k == "ns::ns-b")
            .map(|(_, v)| v.as_str());
        assert_eq!(ns_a, Some("2"));
        assert_eq!(ns_b, Some("1"));
    }

    #[test]
    fn local_run_index_section_reports_hnsw_estimate() {
        let env = TestEnv::fresh();
        seed_memory(&env.db_path, "ns", "t1", "c1");
        let report = run_local_collect(&env.db_path);
        let index = find(&report, "Index");
        // seed_memory does not write an embedding so hnsw_size_estimate=0.
        assert_eq!(fact(index, "hnsw_size_estimate"), "0");
        // Cold-start estimate is rendered with two decimals.
        let cs = fact(index, "cold_start_rebuild_secs_estimate");
        assert!(
            cs.contains('.'),
            "cold_start_secs_estimate should be float-like, got {cs}"
        );
        assert_eq!(index.severity, Severity::Info);
    }

    #[test]
    fn local_run_recall_section_documents_pre_p3_state() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let recall = find(&report, "Recall");
        assert_eq!(recall.severity, Severity::Info);
        assert!(fact(recall, "recall_mode_distribution").contains("pre-P3"));
        assert!(fact(recall, "reranker_used_distribution").contains("pre-P3"));
        // Hint nudges the operator toward --remote for the live feed.
        assert!(fact(recall, "hint").contains("--remote"));
    }

    #[test]
    fn local_run_sync_section_n_a_when_no_peers() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let sync = find(&report, "Sync");
        // Empty sync_state => NotAvailable + note.
        assert_eq!(sync.severity, Severity::NotAvailable);
        assert_eq!(fact(sync, "peer_count"), "0");
        assert!(sync.note.is_some());
    }

    #[test]
    fn local_run_capabilities_local_section_n_a() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::NotAvailable);
        assert!(fact(cap, "capabilities").contains("--remote"));
    }

    #[test]
    fn local_run_governance_section_empty_is_info() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let gov = find(&report, "Governance");
        assert_eq!(gov.severity, Severity::Info);
        assert_eq!(fact(gov, "namespaces_with_policy"), "0");
        assert_eq!(fact(gov, "namespaces_without_policy"), "0");
        assert_eq!(fact(gov, "inheritance_depth"), "empty");
        assert_eq!(fact(gov, "oldest_pending_age_secs"), "queue_empty");
        assert_eq!(fact(gov, "pending_actions_total"), "0");
    }

    #[test]
    fn local_run_webhook_section_empty_no_deliveries() {
        let env = TestEnv::fresh();
        let report = run_local_collect(&env.db_path);
        let wh = find(&report, "Webhook");
        assert_eq!(wh.severity, Severity::Info);
        assert_eq!(fact(wh, "subscription_count"), "0");
        assert_eq!(fact(wh, "dispatched_total"), "0");
        assert_eq!(fact(wh, "failed_total"), "0");
        assert_eq!(fact(wh, "success_rate_pct"), "no_deliveries_yet");
    }

    // -------------------------------------------------------------------
    // Severity rule cases — DB-backed
    // -------------------------------------------------------------------

    #[test]
    fn governance_section_critical_when_pending_older_than_24h() {
        let env = TestEnv::fresh();
        // Open the DB once to materialize schema, then write a pending row.
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let twenty_five_hours_ago =
                (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
            conn.execute(
                "INSERT INTO pending_actions \
                 (id, action_type, namespace, payload, requested_by, requested_at, status) \
                 VALUES ('p1', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
                params![twenty_five_hours_ago],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let gov = find(&report, "Governance");
        assert_eq!(gov.severity, Severity::Critical);
        assert!(gov.note.as_ref().unwrap().contains("24h"));
        // pending_actions_total reflects the row.
        assert_eq!(fact(gov, "pending_actions_total"), "1");
        // overall picks the Critical from Governance.
        assert_eq!(report.overall, Severity::Critical);
    }

    #[test]
    fn governance_section_info_when_pending_younger_than_24h() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let one_hour_ago = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
            conn.execute(
                "INSERT INTO pending_actions \
                 (id, action_type, namespace, payload, requested_by, requested_at, status) \
                 VALUES ('p2', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
                params![one_hour_ago],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let gov = find(&report, "Governance");
        // 1h pending — under the 24h threshold; Info, no critical bump.
        assert_eq!(gov.severity, Severity::Info);
        assert_eq!(fact(gov, "pending_actions_total"), "1");
        // The age fact is set to a numeric string, not "queue_empty".
        let age_str = fact(gov, "oldest_pending_age_secs");
        assert!(
            age_str.parse::<i64>().is_ok(),
            "expected numeric age, got {age_str}"
        );
    }

    #[test]
    fn sync_section_critical_when_skew_exceeds_600s() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            // last_seen_at = now, last_pulled_at = 1 hour ago → 3600s skew.
            let now = chrono::Utc::now();
            let now_s = now.to_rfc3339();
            let earlier = (now - chrono::Duration::seconds(3600)).to_rfc3339();
            conn.execute(
                "INSERT INTO sync_state (agent_id, peer_id, last_seen_at, last_pulled_at) \
                 VALUES ('me', 'peer-1', ?1, ?2)",
                params![now_s, earlier],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let sync = find(&report, "Sync");
        assert_eq!(sync.severity, Severity::Critical);
        assert!(sync.note.as_ref().unwrap().contains("600s"));
        assert_eq!(fact(sync, "peer_count"), "1");
        assert_eq!(report.overall, Severity::Critical);
    }

    #[test]
    fn sync_section_info_when_skew_under_threshold() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let now = chrono::Utc::now();
            let now_s = now.to_rfc3339();
            let close = (now - chrono::Duration::seconds(60)).to_rfc3339();
            conn.execute(
                "INSERT INTO sync_state (agent_id, peer_id, last_seen_at, last_pulled_at) \
                 VALUES ('me', 'peer-1', ?1, ?2)",
                params![now_s, close],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let sync = find(&report, "Sync");
        assert_eq!(sync.severity, Severity::Info);
        // peer_count=1, skew column rendered as a numeric string.
        assert_eq!(fact(sync, "peer_count"), "1");
        let skew = fact(sync, "max_skew_secs");
        assert!(
            skew.parse::<i64>().is_ok(),
            "expected numeric skew, got {skew}"
        );
    }

    #[test]
    fn webhook_section_warning_when_success_rate_below_95() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            // 100 dispatches, 10 failures = 90% success → < 95% threshold.
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO subscriptions \
                 (id, url, events, created_at, dispatch_count, failure_count) \
                 VALUES ('s1', 'http://example/x', '*', ?1, 100, 10)",
                params![now],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let wh = find(&report, "Webhook");
        assert_eq!(wh.severity, Severity::Warning);
        assert!(wh.note.as_ref().unwrap().contains("95%"));
        assert_eq!(fact(wh, "subscription_count"), "1");
        assert_eq!(fact(wh, "dispatched_total"), "100");
        assert_eq!(fact(wh, "failed_total"), "10");
        assert_eq!(fact(wh, "success_rate_pct"), "90.00");
    }

    #[test]
    fn webhook_section_info_when_success_rate_at_or_above_95() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            // 100 dispatches, 3 failures = 97% success.
            conn.execute(
                "INSERT INTO subscriptions \
                 (id, url, events, created_at, dispatch_count, failure_count) \
                 VALUES ('s1', 'http://example/x', '*', ?1, 100, 3)",
                params![now],
            )
            .unwrap();
        }
        let report = run_local_collect(&env.db_path);
        let wh = find(&report, "Webhook");
        assert_eq!(wh.severity, Severity::Info);
        assert!(wh.note.is_none());
        assert_eq!(fact(wh, "success_rate_pct"), "97.00");
    }

    #[test]
    fn governance_section_with_namespace_chain_reports_depths() {
        let env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            for (ns, parent) in [
                ("root", None::<&str>),
                ("a", Some("root")),
                ("a/b", Some("a")),
            ] {
                conn.execute(
                    "INSERT INTO namespace_meta (namespace, parent_namespace, updated_at) \
                     VALUES (?1, ?2, ?3)",
                    params![ns, parent, now],
                )
                .unwrap();
            }
        }
        let report = run_local_collect(&env.db_path);
        let gov = find(&report, "Governance");
        assert_eq!(gov.severity, Severity::Info);
        let depth = fact(gov, "inheritance_depth");
        assert!(depth.contains("d0=") && depth.contains("d1=") && depth.contains("d2="));
        assert_eq!(fact(gov, "namespaces_without_policy"), "3");
    }

    // -------------------------------------------------------------------
    // run() entry point — JSON / text / exit code branches
    // -------------------------------------------------------------------

    #[test]
    fn run_emits_json_when_json_flag_set() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: true,
                fail_on_warn: false,
            },
            &mut out,
        )
        .unwrap();
        // Healthy fresh DB → exit 0.
        assert_eq!(exit, 0);
        let s = env.stdout_str();
        let v: serde_json::Value = serde_json::from_str(s).expect("JSON output must parse");
        assert_eq!(v["mode"], "local");
        assert!(v["sections"].is_array());
        assert!(v["overall"].is_string());
    }

    #[test]
    fn run_emits_text_by_default() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: false,
                fail_on_warn: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0);
        let s = env.stdout_str();
        // Header + section labels.
        assert!(s.contains("ai-memory doctor — local mode"));
        assert!(s.contains("[INFO] Storage"));
        assert!(s.contains("[INFO] Index"));
        assert!(s.contains("[N/A ] Capabilities"));
        // The label-prefixed fact key column is left-padded to 32 chars
        // (smoke check that the format string compiles).
        assert!(s.contains("total_memories"));
    }

    #[test]
    fn run_returns_exit_2_on_critical() {
        let mut env = TestEnv::fresh();
        // Inject a 25h-old pending action → Governance CRIT → overall CRIT.
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let twenty_five_hours_ago =
                (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
            conn.execute(
                "INSERT INTO pending_actions \
                 (id, action_type, namespace, payload, requested_by, requested_at, status) \
                 VALUES ('p1', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
                params![twenty_five_hours_ago],
            )
            .unwrap();
        }
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: true,
                fail_on_warn: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 2);
        // JSON overall is "critical".
        let v: serde_json::Value = serde_json::from_str(env.stdout_str()).unwrap();
        assert_eq!(v["overall"], "critical");
    }

    #[test]
    fn run_warning_keeps_exit_0_without_fail_on_warn() {
        let mut env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO subscriptions \
                 (id, url, events, created_at, dispatch_count, failure_count) \
                 VALUES ('s1', 'http://x', '*', ?1, 10, 5)",
                params![now],
            )
            .unwrap();
        }
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: false,
                fail_on_warn: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 0, "warning without --fail-on-warn must keep exit 0");
        assert!(env.stdout_str().contains("[WARN] Webhook"));
    }

    #[test]
    fn run_warning_returns_exit_1_with_fail_on_warn() {
        let mut env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            conn.execute(
                "INSERT INTO subscriptions \
                 (id, url, events, created_at, dispatch_count, failure_count) \
                 VALUES ('s1', 'http://x', '*', ?1, 10, 5)",
                params![now],
            )
            .unwrap();
        }
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: false,
                fail_on_warn: true,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 1, "--fail-on-warn must promote warning to exit 1");
    }

    #[test]
    fn run_critical_is_exit_2_even_without_fail_on_warn() {
        let mut env = TestEnv::fresh();
        {
            let conn = crate::db::open(&env.db_path).unwrap();
            let twenty_five_hours_ago =
                (chrono::Utc::now() - chrono::Duration::hours(25)).to_rfc3339();
            conn.execute(
                "INSERT INTO pending_actions \
                 (id, action_type, namespace, payload, requested_by, requested_at, status) \
                 VALUES ('p1', 'store', 'ns', '{}', 'agent', ?1, 'pending')",
                params![twenty_five_hours_ago],
            )
            .unwrap();
        }
        let db_path = env.db_path.clone();
        let mut out = env.output();
        let exit = run(
            &db_path,
            &DoctorArgs {
                remote: None,
                json: false,
                fail_on_warn: false,
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(exit, 2);
    }

    // -------------------------------------------------------------------
    // run() — corrupt DB path: db::open() fails → CRITICAL Storage section.
    // -------------------------------------------------------------------

    #[test]
    fn local_run_on_unopenable_db_returns_critical_storage_only() {
        let tmp = tempfile::tempdir().unwrap();
        let bad = tmp.path().join("not-a-db.db");
        // Write garbage so SQLite refuses to open it.
        std::fs::write(&bad, b"this is not a sqlite database, it's just text").unwrap();
        let report = run_local_collect(&bad);
        // The error path appends a single Storage section and returns.
        assert_eq!(report.sections.len(), 1);
        let storage = &report.sections[0];
        assert_eq!(storage.name, "Storage");
        assert_eq!(storage.severity, Severity::Critical);
        // overall is computed from the single section.
        assert_eq!(report.overall, Severity::Critical);
        assert!(storage.note.as_ref().unwrap().contains("could not open"));
    }

    // -------------------------------------------------------------------
    // Render helpers
    // -------------------------------------------------------------------

    #[test]
    fn render_text_emits_section_note_when_present() {
        let r = mk_report(vec![ReportSection {
            name: "Sync".into(),
            severity: Severity::Critical,
            facts: vec![("max_skew_secs".into(), "9999".into())],
            note: Some("peer mesh is drifting".into()),
        }]);
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
        render_text(&r, &mut out).unwrap();
        let s = String::from_utf8(stdout).unwrap();
        assert!(s.contains("[CRIT] Sync"));
        assert!(s.contains("note: peer mesh is drifting"));
        assert!(s.contains("max_skew_secs"));
        assert!(s.contains("9999"));
    }

    // -------------------------------------------------------------------
    // Remote (--remote) mode — wiremock-driven HTTP fixtures
    // -------------------------------------------------------------------

    /// Helper: run `run_remote` from a multi-thread tokio test by spawning
    /// the blocking reqwest call onto the spawn_blocking pool.
    async fn run_remote_in_blocking(url: String, db_path: PathBuf) -> Report {
        tokio::task::spawn_blocking(move || {
            let mut r = run_remote(&url, &db_path);
            r.compute_overall();
            r
        })
        .await
        .unwrap()
    }

    use std::path::PathBuf;

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_section_capabilities_parses_v2_fields() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "feature_tier": "smart",
                "features": {
                    "recall_mode_active": "hybrid",
                    "reranker_active": "cross_encoder"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": 42,
                "expiring_soon": 1,
                "links_count": 3
            })))
            .mount(&server)
            .await;

        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(server.uri(), env.db_path.clone()).await;
        assert_eq!(report.mode, "remote");
        assert!(report.source.starts_with(&server.uri()));
        // Sections: 7 total — Capabilities, Recall, Storage, Index, Governance, Sync, Webhook.
        assert_eq!(report.sections.len(), 7);

        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::Info);
        assert_eq!(fact(cap, "schema_version"), "2");
        assert_eq!(fact(cap, "recall_mode_active"), "hybrid");
        assert_eq!(fact(cap, "reranker_active"), "cross_encoder");

        let recall = find(&report, "Recall");
        assert_eq!(fact(recall, "active_recall_mode"), "hybrid");
        assert_eq!(fact(recall, "active_reranker"), "cross_encoder");

        let storage = find(&report, "Storage");
        assert_eq!(fact(storage, "total_memories"), "42");
        assert_eq!(fact(storage, "expiring_within_1h"), "1");
        assert_eq!(fact(storage, "links"), "3");

        // Raw-SQL sections must be NotAvailable in remote mode.
        for raw in ["Index", "Governance", "Sync", "Webhook"] {
            let s = find(&report, raw);
            assert_eq!(s.severity, Severity::NotAvailable);
            assert!(fact(s, "hint").contains("--db mode"));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_capabilities_silent_degrade_warns_on_capable_tier() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "feature_tier": "semantic",
                "features": {
                    "recall_mode_active": "keyword_only",
                    "reranker_active": "none"
                }
            })))
            .mount(&server)
            .await;
        // /api/v1/stats not mocked → 404 → Storage carries an error fact
        // but no severity bump (severity stays Info per the code path).
        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(server.uri(), env.db_path.clone()).await;
        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::Warning);
        assert!(cap.note.as_ref().unwrap().contains("silent degradation"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_capabilities_degraded_on_keyword_tier_does_not_warn() {
        // recall_mode=degraded but feature_tier=keyword → no silent-degrade
        // (keyword tier was never expected to run hybrid in the first place).
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "feature_tier": "keyword",
                "features": {
                    "recall_mode_active": "keyword_only",
                    "reranker_active": "none"
                }
            })))
            .mount(&server)
            .await;
        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(server.uri(), env.db_path.clone()).await;
        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::Info);
        assert!(cap.note.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_capabilities_unreachable_endpoint_is_critical() {
        // Reserve a free port and immediately drop the listener so the
        // connection refusal is deterministic. Doctor's HTTP timeout is
        // 5s; the kernel rejects almost immediately so the test stays
        // well under the per-test timeout.
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let url = format!("http://127.0.0.1:{port}");

        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(url, env.db_path.clone()).await;
        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::Critical);
        assert!(cap.note.as_ref().unwrap().contains("could not reach"));
        assert_eq!(report.overall, Severity::Critical);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_capabilities_legacy_v1_renders_not_in_response() {
        // Legacy v0.6.3 capabilities responses don't carry the v2 fields.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "1"
            })))
            .mount(&server)
            .await;
        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(server.uri(), env.db_path.clone()).await;
        let cap = find(&report, "Capabilities");
        // Legacy v1 → no severity bump, but missing fields are rendered.
        assert_eq!(cap.severity, Severity::Info);
        assert_eq!(fact(cap, "schema_version"), "1");
        assert_eq!(fact(cap, "recall_mode_active"), "not_in_response");
        assert_eq!(fact(cap, "reranker_active"), "not_in_response");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_run_via_run_entry_uses_remote_mode_string() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "feature_tier": "semantic",
                "features": {
                    "recall_mode_active": "hybrid",
                    "reranker_active": "none"
                }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "total": 0
            })))
            .mount(&server)
            .await;

        let env_db = TestEnv::fresh().db_path;
        let url = server.uri();
        let (exit, stdout) = tokio::task::spawn_blocking(move || {
            let mut stdout = Vec::<u8>::new();
            let mut stderr = Vec::<u8>::new();
            let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
            let exit = run(
                &env_db,
                &DoctorArgs {
                    remote: Some(url),
                    json: true,
                    fail_on_warn: false,
                },
                &mut out,
            )
            .unwrap();
            (exit, stdout)
        })
        .await
        .unwrap();
        assert_eq!(exit, 0);
        let v: serde_json::Value = serde_json::from_slice(&stdout).unwrap();
        assert_eq!(v["mode"], "remote");
        // Trailing slash on the URL must be normalized.
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_url_trailing_slash_is_trimmed() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "features": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let env = TestEnv::fresh();
        // Append a trailing slash; format!("{base}/api/v1/...") would
        // otherwise produce a `//api/v1/` path that wiremock would 404.
        let report =
            run_remote_in_blocking(format!("{}/", server.uri()), env.db_path.clone()).await;
        let cap = find(&report, "Capabilities");
        assert_eq!(cap.severity, Severity::Info);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn remote_storage_500_renders_error_without_severity_bump() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/capabilities"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "schema_version": "2",
                "features": {}
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/stats"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let env = TestEnv::fresh();
        let report = run_remote_in_blocking(server.uri(), env.db_path.clone()).await;
        let storage = find(&report, "Storage");
        // Storage section preserves Info severity even on 5xx — by spec
        // (remote storage is best-effort; sql truth is the local mode).
        assert_eq!(storage.severity, Severity::Info);
        let err = fact(storage, "error");
        assert!(
            err.contains("HTTP 500"),
            "expected HTTP 500 message, got {err}"
        );
    }

    // ---- v0.6.4-004 — `--tokens` reporter ----

    fn run_tokens_capture(args: TokensArgs) -> (i32, String, String) {
        let mut stdout = Vec::<u8>::new();
        let mut stderr = Vec::<u8>::new();
        let exit;
        {
            let mut out = CliOutput::from_std(&mut stdout, &mut stderr);
            exit = run_tokens(args, &mut out).expect("run_tokens");
        }
        (
            exit,
            String::from_utf8(stdout).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn run_tokens_human_default_profile_is_core() {
        let (exit, stdout, _stderr) = run_tokens_capture(TokensArgs::default());
        assert_eq!(exit, 0);
        assert!(
            stdout.contains("Active profile: core"),
            "default profile should be core; got: {stdout}"
        );
        assert!(
            stdout.contains("Full   (43 tools loaded)"),
            "report should include full-profile baseline"
        );
        assert!(
            stdout.contains("Tokenizer: cl100k_base"),
            "report should call out the tokenizer"
        );
    }

    #[test]
    fn run_tokens_json_emits_structured_payload() {
        let args = TokensArgs {
            json: true,
            raw_table: false,
            profile: Some("graph".to_string()),
            hooks: false,
        };
        let (exit, stdout, _) = run_tokens_capture(args);
        assert_eq!(exit, 0);
        let v: serde_json::Value =
            serde_json::from_str(&stdout).expect("--json must emit valid JSON");
        assert_eq!(v["schema_version"], "v0.6.4-tokens-1");
        assert_eq!(v["tokenizer"], "cl100k_base");
        // Token count grows as schemas evolve. Assert the honest
        // cl100k_base range from sizes.rs (5K-8K) rather than an
        // exact value; the exact-figure invariant lives in
        // `sizes::tests::full_profile_total_in_honest_measured_range`.
        let total = v["full_profile_total_tokens"].as_u64().unwrap();
        assert!(
            (5_000..=8_000).contains(&total),
            "full_profile_total_tokens out of honest range: {total}"
        );
        assert!(v["active_total_tokens"].as_u64().unwrap() > 0);
        // graph profile loads core + graph; both flags true on those rows.
        let families = v["families"].as_array().unwrap();
        let core_row = families.iter().find(|r| r["name"] == "core").unwrap();
        assert_eq!(core_row["loaded"], true);
        let graph_row = families.iter().find(|r| r["name"] == "graph").unwrap();
        assert_eq!(graph_row["loaded"], true);
        let archive_row = families.iter().find(|r| r["name"] == "archive").unwrap();
        assert_eq!(archive_row["loaded"], false);
    }

    #[test]
    fn run_tokens_raw_table_includes_per_tool_rows() {
        let args = TokensArgs {
            json: false,
            raw_table: true,
            profile: None,
            hooks: false,
        };
        let (exit, stdout, _) = run_tokens_capture(args);
        assert_eq!(exit, 0);
        let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            43,
            "raw_table must include all 43 baseline tools"
        );
        // memory_store is in core and must be loaded under the default
        // (core) profile.
        let store = tools
            .iter()
            .find(|t| t["name"] == "memory_store")
            .expect("memory_store row");
        assert_eq!(store["family"], "core");
        assert_eq!(store["loaded_under_active_profile"], true);
    }

    #[test]
    fn run_tokens_invalid_profile_exits_2_with_diagnostic() {
        let args = TokensArgs {
            json: false,
            raw_table: false,
            profile: Some("Core".to_string()),
            hooks: false,
        };
        let (exit, _stdout, stderr) = run_tokens_capture(args);
        assert_eq!(exit, 2, "malformed profile must exit 2");
        assert!(
            stderr.contains("case-sensitive lowercase"),
            "diagnostic should mention case rule; got: {stderr}"
        );
    }
}
