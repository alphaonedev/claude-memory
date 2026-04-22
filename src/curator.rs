// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Autonomous curator daemon (v0.6.1).
//!
//! Runs a periodic sweep over stored memories, invoking `auto_tag` and
//! `detect_contradiction` via the configured LLM and persisting results
//! into each memory's metadata. Complements the synchronous post-store
//! hooks shipped in v0.6.0.0 (#265) — those fire inline on writes; the
//! curator catches memories that were stored before hooks were enabled,
//! or when the LLM was temporarily offline, or that only become
//! interesting later as more context accumulates.
//!
//! The curator is intentionally bounded:
//!
//! - Hard cap on operations per cycle — never runs unbounded work.
//! - Skips internal (`_`-prefixed) namespaces.
//! - Honours include / exclude namespace lists.
//! - Dry-run mode emits the report without touching any row.
//! - Each operation is best-effort; LLM errors are logged but never
//!   abort the cycle.

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::db;
use crate::llm::OllamaClient;
use crate::models::{Memory, Tier};

/// Default curator sweep interval (1 hour).
pub const DEFAULT_INTERVAL_SECS: u64 = 3600;

/// Default per-cycle operation cap (stops runaway LLM calls).
pub const DEFAULT_MAX_OPS_PER_CYCLE: usize = 100;

/// Minimum content length before the curator will touch a memory —
/// matches the synchronous hook threshold in `src/mcp.rs`.
pub const MIN_CONTENT_LEN: usize = 50;

/// Curator configuration (surfaced to CLI + config file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorConfig {
    /// Seconds between sweeps in daemon mode. Clamped at runtime to
    /// `[60, 86400]` to avoid pathological values.
    pub interval_secs: u64,
    /// Hard cap on LLM-invoking operations per cycle.
    pub max_ops_per_cycle: usize,
    /// When true, emits the report but never writes back to the DB.
    pub dry_run: bool,
    /// When non-empty, only these namespaces are curated. Exact match.
    pub include_namespaces: Vec<String>,
    /// Namespaces to skip. Exact match. Always also skips `_`-prefixed.
    pub exclude_namespaces: Vec<String>,
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            interval_secs: DEFAULT_INTERVAL_SECS,
            max_ops_per_cycle: DEFAULT_MAX_OPS_PER_CYCLE,
            dry_run: false,
            include_namespaces: Vec::new(),
            exclude_namespaces: Vec::new(),
        }
    }
}

/// Structured report produced by a single curator cycle. Serialises
/// cleanly to JSON for CLI output, systemd journald, or Prometheus
/// text-format conversion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CuratorReport {
    pub started_at: String,
    pub completed_at: String,
    pub cycle_duration_ms: u128,
    pub memories_scanned: usize,
    pub memories_eligible: usize,
    pub auto_tagged: usize,
    pub contradictions_found: usize,
    pub operations_attempted: usize,
    pub operations_skipped_cap: usize,
    /// v0.6.1 autonomy passes — consolidation, forget-superseded,
    /// priority feedback, rollback-log. All zero when autonomy is not
    /// enabled or not reached for this cycle.
    #[serde(default)]
    pub autonomy: crate::autonomy::AutonomyPassReport,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

impl CuratorReport {
    fn new(dry_run: bool) -> Self {
        let now = chrono::Utc::now().to_rfc3339();
        Self {
            started_at: now.clone(),
            completed_at: now,
            dry_run,
            ..Self::default()
        }
    }
}

/// Run one curator cycle. Safe to call repeatedly. Returns a structured
/// report regardless of outcome — LLM failures are recorded in
/// `report.errors` rather than propagated.
pub fn run_once(
    conn: &Connection,
    llm: Option<&OllamaClient>,
    cfg: &CuratorConfig,
) -> Result<CuratorReport> {
    let mut report = CuratorReport::new(cfg.dry_run);
    let started = Instant::now();

    let CandidateBatch {
        memories: candidates,
        truncated,
    } = collect_candidates(conn, cfg)?;
    report.memories_scanned = candidates.len();
    record_truncation(&mut report, truncated, cfg);

    let eligible: Vec<&Memory> = candidates
        .iter()
        .filter(|m| needs_curation(m, cfg))
        .collect();
    report.memories_eligible = eligible.len();

    let Some(llm_client) = llm else {
        report.errors.push("no LLM client configured".to_string());
        report.completed_at = chrono::Utc::now().to_rfc3339();
        report.cycle_duration_ms = started.elapsed().as_millis();
        return Ok(report);
    };

    for mem in eligible {
        if report.operations_attempted >= cfg.max_ops_per_cycle {
            report.operations_skipped_cap += 1;
            continue;
        }
        report.operations_attempted += 1;

        match llm_client.auto_tag(&mem.title, &mem.content) {
            Ok(tags) if !tags.is_empty() => {
                let tag_list: Vec<String> = tags.into_iter().take(8).collect::<Vec<String>>();
                if !cfg.dry_run
                    && let Err(e) = persist_auto_tags(conn, mem, &tag_list)
                {
                    report
                        .errors
                        .push(format!("auto_tag persist failed for {}: {e}", mem.id));
                    continue;
                }
                report.auto_tagged += 1;
            }
            Ok(_) => {}
            Err(e) => {
                report
                    .errors
                    .push(format!("auto_tag failed for {}: {e}", mem.id));
            }
        }

        // Look for one adjacent memory in the same namespace that could
        // contradict this one. We don't do an N^2 scan — just the nearest
        // sibling by created_at. Broader contradiction analysis remains
        // an explicit `memory_detect_contradiction` call.
        if let Ok(Some(sibling)) = adjacent_memory(conn, mem) {
            match llm_client.detect_contradiction(&mem.content, &sibling.content) {
                Ok(true) => {
                    if !cfg.dry_run
                        && let Err(e) = persist_contradiction(conn, mem, &sibling.id)
                    {
                        report
                            .errors
                            .push(format!("contradiction persist failed for {}: {e}", mem.id));
                        continue;
                    }
                    report.contradictions_found += 1;
                }
                Ok(false) => {}
                Err(e) => {
                    report.errors.push(format!(
                        "detect_contradiction failed ({} vs {}): {e}",
                        mem.id, sibling.id
                    ));
                }
            }
        }
    }

    // v0.6.1 autonomy passes — consolidate, forget-superseded, priority
    // feedback, rollback-log. Only run when the LLM is available
    // (otherwise run_once would have early-returned already).
    let autonomy_candidates: Vec<crate::models::Memory> = candidates
        .iter()
        .filter(|m| needs_curation(m, cfg))
        .cloned()
        .collect();
    let pass_report =
        crate::autonomy::run_autonomy_passes(conn, llm_client, &autonomy_candidates, cfg.dry_run);
    report.errors.extend(pass_report.errors.clone());
    report.autonomy = pass_report;

    report.completed_at = chrono::Utc::now().to_rfc3339();
    report.cycle_duration_ms = started.elapsed().as_millis();

    // Self-report: write the cycle's outcome as a memory in
    // _curator/reports. Never runs in dry-run (we must not touch the
    // DB there). Best-effort — a failure here gets logged but does
    // not fail the cycle.
    if !cfg.dry_run
        && let Err(e) = crate::autonomy::persist_self_report(
            conn,
            report.cycle_duration_ms,
            &report.autonomy,
            report.auto_tagged,
            report.contradictions_found,
            report.errors.len(),
        )
    {
        tracing::warn!("self-report persist failed: {e}");
    }

    crate::metrics::curator_cycle_completed(
        report.operations_attempted,
        report.auto_tagged,
        report.contradictions_found,
        report.errors.len(),
    );

    Ok(report)
}

/// Long-running daemon loop. Polls `shutdown` between cycles so SIGINT
/// / SIGTERM lands cleanly.
///
/// Arguments are taken by value because this function is designed to be
/// handed to `tokio::task::spawn_blocking`, which requires owned data.
#[allow(clippy::needless_pass_by_value)]
pub fn run_daemon(
    db_path: PathBuf,
    llm: Option<Arc<OllamaClient>>,
    cfg: CuratorConfig,
    shutdown: Arc<AtomicBool>,
) {
    let interval = cfg.interval_secs.clamp(60, 86400);
    tracing::info!(
        "curator daemon started (interval={}s, max_ops={}, dry_run={})",
        interval,
        cfg.max_ops_per_cycle,
        cfg.dry_run
    );

    while !shutdown.load(Ordering::Relaxed) {
        match Connection::open(&db_path) {
            Ok(conn) => {
                let llm_ref = llm.as_deref();
                match run_once(&conn, llm_ref, &cfg) {
                    Ok(report) => tracing::info!(
                        "curator cycle: scanned={} eligible={} tagged={} contradictions={} errors={} ({}ms, dry_run={})",
                        report.memories_scanned,
                        report.memories_eligible,
                        report.auto_tagged,
                        report.contradictions_found,
                        report.errors.len(),
                        report.cycle_duration_ms,
                        report.dry_run
                    ),
                    Err(e) => tracing::error!("curator cycle errored: {e}"),
                }
            }
            Err(e) => tracing::error!("curator could not open db {}: {e}", db_path.display()),
        }

        let deadline = Instant::now() + Duration::from_secs(interval);
        while Instant::now() < deadline {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    tracing::info!("curator daemon shutdown");
}

/// Result of `collect_candidates` — the memories plus a truncation
/// flag so callers can surface "I may have missed rows" in their
/// report rather than silently dropping (#300 item 3 fix).
pub(crate) struct CandidateBatch {
    pub memories: Vec<Memory>,
    /// True iff at least one tier hit the `max_ops_per_cycle * 4` cap;
    /// callers should add a `report.errors` note so operators notice.
    pub truncated: bool,
}

/// Append a truncation warning to the report when `collect_candidates`
/// hit its per-tier cap. Extracted as a helper so `run_once` stays
/// under clippy's `too_many_lines` ceiling.
fn record_truncation(report: &mut CuratorReport, truncated: bool, cfg: &CuratorConfig) {
    if truncated {
        report.errors.push(format!(
            "collect_candidates truncated at cap={} per tier; consider raising max_ops_per_cycle or paginating across cycles",
            cfg.max_ops_per_cycle.saturating_mul(4)
        ));
    }
}

fn collect_candidates(conn: &Connection, cfg: &CuratorConfig) -> Result<CandidateBatch> {
    // We sweep mid + long tier only. Short tier is too volatile — it'll
    // likely be GC'd before the next curator cycle anyway.
    let cap = cfg.max_ops_per_cycle.saturating_mul(4);
    let mut out = Vec::new();
    let mut truncated = false;
    for tier in [Tier::Mid, Tier::Long] {
        let batch = db::list(
            conn,
            None,
            Some(&tier),
            cap,
            0,
            None,
            None,
            None,
            None,
            None,
        )?;
        if batch.len() >= cap {
            // We can't tell from db::list whether there were strictly
            // more than cap rows without a second probe, so treat
            // cap-saturation as definitely-truncated. False positives
            // are acceptable here — a single-line error entry is
            // cheap, silent data loss is not.
            truncated = true;
        }
        out.extend(batch);
    }
    Ok(CandidateBatch {
        memories: out,
        truncated,
    })
}

fn needs_curation(mem: &Memory, cfg: &CuratorConfig) -> bool {
    if mem.namespace.starts_with('_') {
        return false;
    }
    if !cfg.include_namespaces.is_empty() && !cfg.include_namespaces.contains(&mem.namespace) {
        return false;
    }
    if cfg.exclude_namespaces.contains(&mem.namespace) {
        return false;
    }
    if mem.content.len() < MIN_CONTENT_LEN {
        return false;
    }
    // Skip memories that already carry `auto_tags` — the synchronous hook
    // or a previous curator cycle has processed them. The contradiction
    // pass also skips re-examining the same pair: `confirmed_contradictions`
    // presence is the sentinel.
    let has_auto_tags = mem
        .metadata
        .get("auto_tags")
        .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
    !has_auto_tags
}

fn persist_auto_tags(conn: &Connection, mem: &Memory, tags: &[String]) -> Result<()> {
    let mut updated = mem.metadata.clone();
    if let Some(obj) = updated.as_object_mut() {
        obj.insert("auto_tags".to_string(), serde_json::json!(tags));
        obj.insert(
            "curated_at".to_string(),
            serde_json::json!(chrono::Utc::now().to_rfc3339()),
        );
    }
    db::update(
        conn,
        &mem.id,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&updated),
    )?;
    Ok(())
}

fn persist_contradiction(conn: &Connection, mem: &Memory, against_id: &str) -> Result<()> {
    let mut updated = mem.metadata.clone();
    if let Some(obj) = updated.as_object_mut() {
        let existing = obj
            .get("confirmed_contradictions")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut ids: Vec<String> = existing
            .into_iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();
        if !ids.iter().any(|id| id == against_id) {
            ids.push(against_id.to_string());
        }
        obj.insert(
            "confirmed_contradictions".to_string(),
            serde_json::json!(ids),
        );
    }
    db::update(
        conn,
        &mem.id,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(&updated),
    )?;
    Ok(())
}

fn adjacent_memory(conn: &Connection, mem: &Memory) -> Result<Option<Memory>> {
    let batch = db::list(
        conn,
        Some(&mem.namespace),
        None,
        8,
        0,
        None,
        None,
        None,
        None,
        None,
    )?;
    Ok(batch
        .into_iter()
        .find(|m| m.id != mem.id && m.content.len() >= MIN_CONTENT_LEN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_sane_values() {
        let cfg = CuratorConfig::default();
        assert_eq!(cfg.interval_secs, DEFAULT_INTERVAL_SECS);
        assert_eq!(cfg.max_ops_per_cycle, DEFAULT_MAX_OPS_PER_CYCLE);
        assert!(!cfg.dry_run);
        assert!(cfg.include_namespaces.is_empty());
        assert!(cfg.exclude_namespaces.is_empty());
    }

    #[test]
    fn needs_curation_skips_internal_namespaces() {
        let mem = Memory {
            id: "m1".to_string(),
            tier: Tier::Mid,
            namespace: "_messages/alice".to_string(),
            title: "t".to_string(),
            content: "a".repeat(100),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        assert!(!needs_curation(&mem, &CuratorConfig::default()));
    }

    #[test]
    fn needs_curation_skips_short_content() {
        let mem = Memory {
            id: "m1".to_string(),
            tier: Tier::Mid,
            namespace: "app".to_string(),
            title: "t".to_string(),
            content: "short".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        assert!(!needs_curation(&mem, &CuratorConfig::default()));
    }

    #[test]
    fn needs_curation_skips_already_tagged() {
        let mem = Memory {
            id: "m1".to_string(),
            tier: Tier::Long,
            namespace: "app".to_string(),
            title: "t".to_string(),
            content: "a".repeat(100),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({"auto_tags":["x","y"]}),
        };
        assert!(!needs_curation(&mem, &CuratorConfig::default()));
    }

    #[test]
    fn needs_curation_respects_include_list() {
        let mem = Memory {
            id: "m1".to_string(),
            tier: Tier::Long,
            namespace: "app".to_string(),
            title: "t".to_string(),
            content: "a".repeat(100),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        let mut cfg = CuratorConfig {
            include_namespaces: vec!["other".to_string()],
            ..CuratorConfig::default()
        };
        assert!(!needs_curation(&mem, &cfg));
        cfg.include_namespaces = vec!["app".to_string()];
        assert!(needs_curation(&mem, &cfg));
    }

    #[test]
    fn needs_curation_respects_exclude_list() {
        let mem = Memory {
            id: "m1".to_string(),
            tier: Tier::Long,
            namespace: "noisy".to_string(),
            title: "t".to_string(),
            content: "a".repeat(100),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            last_accessed_at: None,
            expires_at: None,
            metadata: serde_json::json!({}),
        };
        let cfg = CuratorConfig {
            exclude_namespaces: vec!["noisy".to_string()],
            ..CuratorConfig::default()
        };
        assert!(!needs_curation(&mem, &cfg));
    }

    #[test]
    fn run_once_without_llm_emits_error_but_succeeds() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let conn = db::open(tmp.path()).unwrap();
        let cfg = CuratorConfig::default();
        let report = run_once(&conn, None, &cfg).unwrap();
        assert_eq!(report.memories_scanned, 0);
        assert_eq!(report.memories_eligible, 0);
        assert_eq!(report.operations_attempted, 0);
        assert!(report.errors.iter().any(|e| e.contains("no LLM")));
    }

    #[test]
    fn report_serialises_to_json() {
        let report = CuratorReport::new(true);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("dry_run"));
        assert!(json.contains("memories_scanned"));
    }
}
