// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `ai-memory logs` — operator-facing CLI for the file logging facility
//! (PR-5 of issue #487). Tail, archive, purge, filter.
//!
//! The subcommand operates on the directory configured by
//! `[logging] path = ...` in `config.toml`. When logging is disabled
//! the commands still work against the empty default directory and
//! exit 0 — the CLI is a no-op rather than a hard error so a fresh
//! install isn't surprised.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use clap::{Args, Subcommand};

use crate::cli::CliOutput;
use crate::config::{AppConfig, LoggingConfig};
use crate::log_paths;
use crate::logging::resolve_log_dir_with_override;

#[derive(Args)]
pub struct LogsArgs {
    #[command(subcommand)]
    pub action: LogsAction,
    /// RFC3339 lower bound. Lines older than this are dropped from
    /// `tail` / `cat` output.
    #[arg(long, global = true, value_name = "TS")]
    pub since: Option<String>,
    /// RFC3339 upper bound.
    #[arg(long, global = true, value_name = "TS")]
    pub until: Option<String>,
    /// Filter to lines whose tracing `level` field equals this.
    #[arg(long, global = true)]
    pub level: Option<String>,
    /// Filter to lines that mention this namespace (case-insensitive
    /// substring match against the line body).
    #[arg(long, global = true)]
    pub namespace: Option<String>,
    /// Filter to lines that mention this actor / agent_id.
    #[arg(long, global = true)]
    pub actor: Option<String>,
    /// Filter to lines that mention this audit `action`.
    #[arg(long, global = true)]
    pub action_filter: Option<String>,
    /// Output format: `text` (passthrough) or `json` (one filtered
    /// line per JSON object).
    #[arg(long, global = true, default_value = "text")]
    pub format: String,
    /// Override the operational log directory. Highest-priority layer
    /// in the resolution ladder (CLI > `AI_MEMORY_LOG_DIR` > `[logging]
    /// path` in config.toml > platform default). Refuses world-writable
    /// directories — see `docs/security/audit-trail.md`.
    #[arg(long, global = true, value_name = "PATH")]
    pub log_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum LogsAction {
    /// Print recent log lines and (with `--follow`) stream new ones.
    Tail(TailArgs),
    /// Print every log line in chronological order, applying any
    /// global filters.
    Cat,
    /// Compress rotated log files older than the configured
    /// `retention_days` using zstd.
    Archive,
    /// Delete archived log files older than `--before <date>`. Warns
    /// when the date overlaps the audit retention horizon (deleting
    /// audit logs creates an audit gap).
    Purge(PurgeArgs),
}

#[derive(Args)]
pub struct TailArgs {
    /// Number of recent lines to print before tailing. Default 50.
    #[arg(long, default_value_t = 50)]
    pub lines: usize,
    /// Stream new lines as they arrive (poll-based, ~1s cadence).
    #[arg(long, default_value_t = false)]
    pub follow: bool,
    /// Override the poll interval in milliseconds. Default 1000.
    #[arg(long, default_value_t = 1000)]
    pub follow_interval_ms: u64,
    /// Stop tailing after this many polls in `--follow` mode. Tests
    /// pass a small bound so the loop terminates deterministically.
    /// 0 = no bound.
    #[arg(long, default_value_t = 0, hide = true)]
    pub max_polls: u64,
}

#[derive(Args)]
pub struct PurgeArgs {
    /// Delete archives whose mtime is older than this RFC3339 date.
    #[arg(long, value_name = "DATE")]
    pub before: String,
    /// Suppress the audit-gap warning even when audit logs would be
    /// deleted. Reserved for automated rotation pipelines.
    #[arg(long, default_value_t = false)]
    pub no_warn: bool,
    /// Dry run — print which files would be deleted without
    /// removing them.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// `ai-memory logs` entry point.
pub fn run(args: LogsArgs, app_config: &AppConfig, out: &mut CliOutput<'_>) -> Result<()> {
    let logging_cfg = app_config.effective_logging();
    let resolved = resolve_log_dir_with_override(args.log_dir.as_deref(), &logging_cfg)
        .with_context(|| "resolving operational log directory")?;
    let dir = resolved.path.clone();
    let _source: log_paths::PathSource = resolved.source;
    let filters = args_filters(&args);
    match args.action {
        LogsAction::Tail(t) => run_tail(&dir, &filters, &t, out),
        LogsAction::Cat => run_cat(&dir, &filters, out),
        LogsAction::Archive => run_archive(&dir, &logging_cfg, out),
        LogsAction::Purge(p) => run_purge(&dir, &p, app_config, out),
    }
}

#[derive(Default, Clone)]
struct Filters {
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    level: Option<String>,
    namespace: Option<String>,
    actor: Option<String>,
    action: Option<String>,
    format_json: bool,
}

fn args_filters(a: &LogsArgs) -> Filters {
    Filters {
        since: a.since.as_deref().and_then(parse_ts),
        until: a.until.as_deref().and_then(parse_ts),
        level: a.level.clone(),
        namespace: a.namespace.clone(),
        actor: a.actor.clone(),
        action: a.action_filter.clone(),
        format_json: a.format == "json",
    }
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = NaiveDateTime::new(d, NaiveTime::from_hms_opt(0, 0, 0).unwrap());
        return Some(Utc.from_utc_datetime(&dt));
    }
    None
}

fn line_matches(line: &str, filters: &Filters) -> bool {
    if let Some(level) = &filters.level
        && !line
            .to_ascii_uppercase()
            .contains(&level.to_ascii_uppercase())
    {
        return false;
    }
    if let Some(ns) = &filters.namespace
        && !line.to_ascii_lowercase().contains(&ns.to_ascii_lowercase())
    {
        return false;
    }
    if let Some(actor) = &filters.actor
        && !line
            .to_ascii_lowercase()
            .contains(&actor.to_ascii_lowercase())
    {
        return false;
    }
    if let Some(action) = &filters.action
        && !line
            .to_ascii_lowercase()
            .contains(&action.to_ascii_lowercase())
    {
        return false;
    }
    if filters.since.is_some() || filters.until.is_some() {
        // Best-effort: scan the line for an RFC3339 prefix or a
        // `"timestamp":"…"` JSON field.
        let ts = extract_timestamp(line);
        if let Some(ts) = ts {
            if let Some(since) = filters.since
                && ts < since
            {
                return false;
            }
            if let Some(until) = filters.until
                && ts > until
            {
                return false;
            }
        }
    }
    true
}

fn extract_timestamp(line: &str) -> Option<DateTime<Utc>> {
    // Try a leading RFC3339 token.
    if let Some(stop) = line.find(' ') {
        let head = &line[..stop];
        if let Ok(dt) = DateTime::parse_from_rfc3339(head) {
            return Some(dt.with_timezone(&Utc));
        }
    }
    // JSON form.
    if let Some(idx) = line.find("\"timestamp\":\"") {
        let rest = &line[idx + 13..];
        if let Some(end) = rest.find('"') {
            if let Ok(dt) = DateTime::parse_from_rfc3339(&rest[..end]) {
                return Some(dt.with_timezone(&Utc));
            }
        }
    }
    None
}

/// Enumerate every `ai-memory.log*` file in `dir`, sorted by name
/// (which sorts by date because the rolling appender's suffix is
/// `YYYY-MM-DD[-HH[-MM]]`).
fn enumerate_log_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let p = entry.path();
        if p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.contains("ai-memory") && !n.ends_with(".zst"))
        {
            files.push(p);
        }
    }
    files.sort();
    Ok(files)
}

fn run_cat(dir: &Path, filters: &Filters, out: &mut CliOutput<'_>) -> Result<()> {
    for f in enumerate_log_files(dir)? {
        emit_file(&f, filters, out)?;
    }
    Ok(())
}

fn emit_file(path: &Path, filters: &Filters, out: &mut CliOutput<'_>) -> Result<()> {
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    for line in BufReader::new(f).lines() {
        let line = line?;
        if !line_matches(&line, filters) {
            continue;
        }
        emit_line(&line, filters, out)?;
    }
    Ok(())
}

fn emit_line(line: &str, filters: &Filters, out: &mut CliOutput<'_>) -> Result<()> {
    if filters.format_json {
        // If the line is already JSON pass through; otherwise wrap
        // it so downstream `jq` always sees an object.
        if line.trim_start().starts_with('{') {
            writeln!(out.stdout, "{line}")?;
        } else {
            let v = serde_json::json!({ "line": line });
            writeln!(out.stdout, "{}", serde_json::to_string(&v)?)?;
        }
    } else {
        writeln!(out.stdout, "{line}")?;
    }
    Ok(())
}

fn run_tail(dir: &Path, filters: &Filters, args: &TailArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let files = enumerate_log_files(dir)?;
    let Some(latest) = files.last().cloned() else {
        return Ok(());
    };
    // Read the tail-N matching lines.
    let initial = read_tail_n(&latest, args.lines, filters)?;
    for line in &initial {
        emit_line(line, filters, out)?;
    }
    if !args.follow {
        return Ok(());
    }
    let mut last_size = fs::metadata(&latest).map(|m| m.len()).unwrap_or(0);
    let mut polls: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_millis(args.follow_interval_ms));
        polls += 1;
        let cur_size = fs::metadata(&latest).map(|m| m.len()).unwrap_or(last_size);
        if cur_size > last_size {
            let new_lines = read_lines_after_offset(&latest, last_size)?;
            for line in new_lines {
                if line_matches(&line, filters) {
                    emit_line(&line, filters, out)?;
                }
            }
            last_size = cur_size;
        }
        if args.max_polls > 0 && polls >= args.max_polls {
            return Ok(());
        }
    }
}

fn read_tail_n(path: &Path, n: usize, filters: &Filters) -> Result<Vec<String>> {
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let buf = BufReader::new(f);
    let mut keep: Vec<String> = Vec::with_capacity(n);
    for line in buf.lines() {
        let line = line?;
        if !line_matches(&line, filters) {
            continue;
        }
        keep.push(line);
        if keep.len() > n {
            keep.remove(0);
        }
    }
    Ok(keep)
}

fn read_lines_after_offset(path: &Path, offset: u64) -> Result<Vec<String>> {
    use std::io::Seek as _;
    use std::io::SeekFrom;
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    f.seek(SeekFrom::Start(offset))?;
    let buf = BufReader::new(f);
    let mut out = Vec::new();
    for line in buf.lines() {
        out.push(line?);
    }
    Ok(out)
}

fn run_archive(dir: &Path, cfg: &LoggingConfig, out: &mut CliOutput<'_>) -> Result<()> {
    let retention_days = i64::from(cfg.retention_days.unwrap_or(90));
    let cutoff = Utc::now() - chrono::Duration::days(retention_days);
    let mut compressed: u64 = 0;
    let mut total_in: u64 = 0;
    let mut total_out: u64 = 0;

    for f in enumerate_log_files(dir)? {
        let mtime = fs::metadata(&f)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| Utc.timestamp_opt(d.as_secs() as i64, 0).unwrap());
        let Some(mtime) = mtime else {
            continue;
        };
        if mtime >= cutoff {
            continue;
        }
        let in_bytes = fs::read(&f).with_context(|| format!("reading {}", f.display()))?;
        let in_size = in_bytes.len() as u64;
        let out_path = f.with_extension(format!(
            "{}.zst",
            f.extension().and_then(|e| e.to_str()).unwrap_or("log")
        ));
        let compressed_bytes = zstd_compress(&in_bytes)?;
        let out_size = compressed_bytes.len() as u64;
        fs::write(&out_path, &compressed_bytes)
            .with_context(|| format!("writing {}", out_path.display()))?;
        fs::remove_file(&f).with_context(|| format!("removing {}", f.display()))?;
        compressed += 1;
        total_in += in_size;
        total_out += out_size;
    }
    writeln!(
        out.stdout,
        "archived {compressed} log file(s): {total_in} bytes -> {total_out} bytes"
    )?;
    Ok(())
}

fn zstd_compress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 + 64);
    {
        let mut encoder = zstd::stream::write::Encoder::new(&mut out, 3)?;
        encoder.write_all(input)?;
        encoder.finish()?;
    }
    Ok(out)
}

fn run_purge(
    dir: &Path,
    args: &PurgeArgs,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let cutoff = parse_ts(&args.before)
        .ok_or_else(|| anyhow!("invalid --before date: {} (expected RFC3339)", args.before))?;
    if !args.no_warn {
        warn_about_audit_gap(args, app_config, out)?;
    }
    if !dir.exists() {
        return Ok(());
    }
    let mut deleted: u64 = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if !p.to_string_lossy().ends_with(".zst") {
            continue;
        }
        let mtime = fs::metadata(&p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| Utc.timestamp_opt(d.as_secs() as i64, 0).unwrap());
        if let Some(mt) = mtime
            && mt < cutoff
        {
            if args.dry_run {
                writeln!(out.stdout, "would delete: {}", p.display())?;
            } else {
                fs::remove_file(&p).with_context(|| format!("removing {}", p.display()))?;
                writeln!(out.stdout, "deleted: {}", p.display())?;
            }
            deleted += 1;
        }
    }
    writeln!(out.stdout, "purged {deleted} archive(s)")?;
    Ok(())
}

fn warn_about_audit_gap(
    args: &PurgeArgs,
    app_config: &AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let audit = app_config.effective_audit();
    if !audit.enabled.unwrap_or(false) {
        return Ok(());
    }
    let retention = audit.effective_retention_days();
    let cutoff = parse_ts(&args.before).unwrap_or_else(Utc::now);
    let oldest_required = Utc::now() - chrono::Duration::days(i64::from(retention));
    if cutoff > oldest_required {
        writeln!(
            out.stderr,
            "warning: --before {ts} would delete archives newer than the configured \
             audit retention horizon ({retention} days, oldest required = {oldest}). \
             Continuing creates an audit gap that `ai-memory audit verify` will surface. \
             Pass --no-warn to suppress this message in automated rotation pipelines.",
            ts = args.before,
            retention = retention,
            oldest = oldest_required.to_rfc3339()
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_log(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, contents).unwrap();
        p
    }

    fn output<'a>(stdout: &'a mut Vec<u8>, stderr: &'a mut Vec<u8>) -> CliOutput<'a> {
        CliOutput::from_std(stdout, stderr)
    }

    #[test]
    fn logs_tail_returns_last_n_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body = (1..=100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        make_log(dir.path(), "ai-memory.log.2026-04-30", &body);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut out = output(&mut stdout, &mut stderr);
            let filters = Filters::default();
            let args = TailArgs {
                lines: 5,
                follow: false,
                follow_interval_ms: 50,
                max_polls: 0,
            };
            run_tail(dir.path(), &filters, &args, &mut out).unwrap();
        }
        let s = std::str::from_utf8(&stdout).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines.last().unwrap(), &"line 100");
    }

    #[test]
    fn logs_tail_follows_appended_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_log(dir.path(), "ai-memory.log.2026-04-30", "first\n");
        let path_clone = path.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(&path_clone)
                .unwrap();
            writeln!(f, "second").unwrap();
            writeln!(f, "third").unwrap();
        });
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut out = output(&mut stdout, &mut stderr);
            let filters = Filters::default();
            let args = TailArgs {
                lines: 10,
                follow: true,
                follow_interval_ms: 30,
                max_polls: 6,
            };
            run_tail(dir.path(), &filters, &args, &mut out).unwrap();
        }
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("first"), "got: {s}");
        assert!(s.contains("second"), "expected appended line: {s}");
    }

    #[test]
    fn logs_archive_compresses_with_zstd() {
        let dir = tempfile::tempdir().unwrap();
        // Use a retention of 0 days so the archiver picks up the file
        // regardless of its mtime — the file's mtime is "now" since we
        // just created it.
        let body = "x".repeat(8192);
        make_log(dir.path(), "ai-memory.log.2025-01-01", &body);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let cfg = LoggingConfig {
            retention_days: Some(0),
            ..Default::default()
        };
        {
            let mut out = output(&mut stdout, &mut stderr);
            run_archive(dir.path(), &cfg, &mut out).unwrap();
        }
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("archived 1"), "expected archive count: {s}");
        // The .zst output replaces the source.
        let entries: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap_or_default())
            .collect();
        assert!(
            entries.iter().any(|n| n.ends_with(".zst")),
            "expected a .zst entry, got {entries:?}"
        );
    }

    #[test]
    fn logs_purge_warns_about_audit_gap() {
        let dir = tempfile::tempdir().unwrap();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let app_config = AppConfig {
            audit: Some(crate::config::AuditConfig {
                enabled: Some(true),
                retention_days: Some(90),
                ..Default::default()
            }),
            ..Default::default()
        };
        {
            let mut out = output(&mut stdout, &mut stderr);
            let args = PurgeArgs {
                before: Utc::now().to_rfc3339(),
                no_warn: false,
                dry_run: true,
            };
            run_purge(dir.path(), &args, &app_config, &mut out).unwrap();
        }
        let serr = std::str::from_utf8(&stderr).unwrap();
        assert!(
            serr.contains("audit gap"),
            "expected audit-gap warning: {serr}"
        );
    }

    #[test]
    fn logs_filter_namespace_substring() {
        let dir = tempfile::tempdir().unwrap();
        let body = "alpha line\nbeta line ns=widgets\ngamma line";
        make_log(dir.path(), "ai-memory.log.2026-04-30", body);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut out = output(&mut stdout, &mut stderr);
            let filters = Filters {
                namespace: Some("widgets".to_string()),
                ..Default::default()
            };
            run_cat(dir.path(), &filters, &mut out).unwrap();
        }
        let s = std::str::from_utf8(&stdout).unwrap();
        assert!(s.contains("beta line"));
        assert!(!s.contains("alpha line"));
        assert!(!s.contains("gamma line"));
    }

    #[test]
    fn logs_format_json_wraps_text_lines() {
        let dir = tempfile::tempdir().unwrap();
        let body = "plain text line";
        make_log(dir.path(), "ai-memory.log.2026-04-30", body);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        {
            let mut out = output(&mut stdout, &mut stderr);
            let filters = Filters {
                format_json: true,
                ..Default::default()
            };
            run_cat(dir.path(), &filters, &mut out).unwrap();
        }
        let s = std::str::from_utf8(&stdout).unwrap();
        let v: serde_json::Value = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(v["line"], "plain text line");
    }
}
