// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

// clippy allows (test scaffolding): pedantic lints with no behavioral impact.
#![allow(clippy::doc_markdown)]
//! v0.7.0 K8 — `memory_quota_status` MCP-tool wiring test.
//!
//! K8 ships the per-agent quota substrate + the operator-facing
//! `memory_quota_status` tool that surfaces the substrate over MCP.
//! This integration test pins:
//!
//! 1. `memory_quota_status` resolves to `Family::Power` so the
//!    `--profile power` operator profile loads it. (Source-anchored at
//!    `src/profile.rs::Family::for_tool`.)
//! 2. `tool_definitions()` registers it (i.e., the tool count cascade
//!    advanced from 50 to 51 — post-B2 rebase, B2's memory_smart_load
//!    landed first on main at 50 so K8 now lifts the count to 51).
//! 3. The handler invoked with `agent_id` returns a single-row
//!    envelope `{ agent_id, quota }`.
//! 4. The handler invoked without `agent_id` returns the full-list
//!    envelope `{ count, quotas: [...] }` sorted by `agent_id` ASC.

use ai_memory::mcp::handle_quota_status;
use ai_memory::profile::{Family, Profile};
use ai_memory::quotas::{self, QuotaOp};
use rusqlite::Connection;
use serde_json::json;
use tempfile::NamedTempFile;

fn fresh_db() -> (NamedTempFile, std::path::PathBuf) {
    let f = NamedTempFile::new().expect("tempfile");
    let p = f.path().to_path_buf();
    let _ = ai_memory::db::open(&p).expect("db::open");
    (f, p)
}

#[test]
fn k8_quota_status_registered_under_power_family() {
    assert_eq!(
        Family::for_tool("memory_quota_status"),
        Some(Family::Power),
        "memory_quota_status must live in Family::Power"
    );

    // Full + power load it; core does not (operator tool).
    assert!(Profile::full().loads("memory_quota_status"));
    assert!(Profile::power().loads("memory_quota_status"));
    assert!(!Profile::core().loads("memory_quota_status"));
}

#[test]
fn k8_quota_status_loaded_under_full_profile() {
    // The tool_definitions() registration walk is exercised via the
    // unit test in src/mcp.rs (tool_definitions_returns_51_tools); here
    // we pin the profile-loading shape from the integration crate.
    assert!(
        Profile::full().loads("memory_quota_status"),
        "full profile must load memory_quota_status (K8 cascade 50 -> 51 post-B2)"
    );
    assert_eq!(
        Profile::full().expected_tool_count(),
        51,
        "tool count cascade must advance to 51 with K8 (post-B2 rebase)"
    );
}

#[test]
fn k8_quota_status_with_agent_id_returns_single_row_envelope() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    quotas::record_op(&conn, "agent-status", QuotaOp::Memory { bytes: 42 }).unwrap();

    let envelope = handle_quota_status(&conn, &json!({"agent_id": "agent-status"}))
        .expect("quota_status with agent_id should succeed");

    assert_eq!(envelope["agent_id"].as_str(), Some("agent-status"));
    let quota = &envelope["quota"];
    assert_eq!(quota["agent_id"].as_str(), Some("agent-status"));
    assert_eq!(quota["current_memories_today"].as_i64(), Some(1));
    assert_eq!(quota["current_storage_bytes"].as_i64(), Some(42));
    assert_eq!(quota["current_links_today"].as_i64(), Some(0));
    assert!(
        quota["max_memories_per_day"].as_i64().unwrap() > 0,
        "default max_memories_per_day must be set"
    );
}

#[test]
fn k8_quota_status_without_agent_id_returns_list_envelope_sorted() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    // Seed three agents in non-alphabetical order so the sort is observable.
    for aid in ["zeta-agent", "alpha-agent", "mu-agent"] {
        quotas::record_op(&conn, aid, QuotaOp::Memory { bytes: 1 }).unwrap();
    }

    let envelope =
        handle_quota_status(&conn, &json!({})).expect("quota_status no-arg should succeed");
    assert_eq!(envelope["count"].as_u64(), Some(3));
    let quotas_arr = envelope["quotas"].as_array().expect("quotas array");
    let ids: Vec<&str> = quotas_arr
        .iter()
        .map(|q| q["agent_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        ids,
        vec!["alpha-agent", "mu-agent", "zeta-agent"],
        "list envelope must be sorted by agent_id ASC"
    );
}

#[test]
fn k8_quota_status_auto_inserts_default_for_unknown_agent() {
    let (_keep, db_path) = fresh_db();
    let conn = Connection::open(&db_path).unwrap();

    let envelope = handle_quota_status(&conn, &json!({"agent_id": "never-seen"}))
        .expect("auto-insert path should succeed");
    let quota = &envelope["quota"];
    assert_eq!(quota["current_memories_today"].as_i64(), Some(0));
    assert_eq!(quota["current_links_today"].as_i64(), Some(0));
    assert_eq!(quota["current_storage_bytes"].as_i64(), Some(0));
}
