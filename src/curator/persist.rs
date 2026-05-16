// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Write-back helpers for the curator sweep.
//!
//! Extracted from the original flat `src/curator.rs` in v0.7.0 Layer
//! 0.5 Task L0.5-1. Pure refactor — no semantic changes. These
//! functions are the only path inside the curator that mutates the
//! database; `run_once` guards every call with a `dry_run` check.

use anyhow::Result;
use rusqlite::Connection;

use crate::db;
use crate::models::Memory;

pub(super) fn persist_auto_tags(conn: &Connection, mem: &Memory, tags: &[String]) -> Result<()> {
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

pub(super) fn persist_contradiction(
    conn: &Connection,
    mem: &Memory,
    against_id: &str,
) -> Result<()> {
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
