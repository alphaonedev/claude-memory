// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Candidate selection helpers for the curator sweep.
//!
//! Extracted from the original flat `src/curator.rs` in v0.7.0 Layer
//! 0.5 Task L0.5-1. Pure refactor — no semantic changes. Contains the
//! per-cycle candidate batch type, the row collector, the
//! eligibility filter, and the adjacent-sibling lookup used by the
//! contradiction pass.

use anyhow::Result;
use rusqlite::Connection;

use crate::db;
use crate::models::{Memory, Tier};

use super::{CuratorConfig, CuratorReport, MIN_CONTENT_LEN};

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
pub(super) fn record_truncation(report: &mut CuratorReport, truncated: bool, cfg: &CuratorConfig) {
    if truncated {
        report.errors.push(format!(
            "collect_candidates truncated at cap={} per tier; consider raising max_ops_per_cycle or paginating across cycles",
            cfg.max_ops_per_cycle.saturating_mul(4)
        ));
    }
}

pub(super) fn collect_candidates(conn: &Connection, cfg: &CuratorConfig) -> Result<CandidateBatch> {
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

pub(super) fn needs_curation(mem: &Memory, cfg: &CuratorConfig) -> bool {
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

pub(super) fn adjacent_memory(conn: &Connection, mem: &Memory) -> Result<Option<Memory>> {
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
