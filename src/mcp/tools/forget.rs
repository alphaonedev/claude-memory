// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_forget` and `memory_stats` handlers.

use crate::db;
use crate::models::Tier;
use serde_json::{Value, json};
use std::path::Path;
pub(super) fn handle_forget(
    conn: &rusqlite::Connection,
    params: &Value,
    archive: bool,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let pattern = params["pattern"].as_str();
    let tier = params["tier"].as_str().and_then(Tier::from_str);
    let dry_run = params["dry_run"].as_bool().unwrap_or(false);

    if dry_run {
        let count =
            db::forget_count(conn, namespace, pattern, tier.as_ref()).map_err(|e| e.to_string())?;
        return Ok(json!({"would_delete": count, "dry_run": true}));
    }

    let deleted =
        db::forget(conn, namespace, pattern, tier.as_ref(), archive).map_err(|e| e.to_string())?;
    Ok(json!({"deleted": deleted, "archived": archive}))
}

pub(super) fn handle_stats(conn: &rusqlite::Connection, db_path: &Path) -> Result<Value, String> {
    let stats = db::stats(conn, db_path).map_err(|e| e.to_string())?;
    serde_json::to_value(stats).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    //! Coverage C-2 — focused tests for `handle_forget` + `handle_stats`.

    use super::*;
    use crate::models::Memory;
    use crate::storage as db;

    fn fresh_conn() -> rusqlite::Connection {
        db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    fn insert_one(conn: &rusqlite::Connection, ns: &str, title: &str, tier: Tier) -> String {
        let now = chrono::Utc::now().to_rfc3339();
        let mem = Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier,
            namespace: ns.to_string(),
            title: title.to_string(),
            content: format!("body for {title}"),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata: json!({"agent_id": "ai:test"}),
            reflection_depth: 0,
            memory_kind: crate::models::MemoryKind::Observation,
            entity_id: None,
            persona_version: None,
            citations: Vec::new(),
            source_uri: None,
            source_span: None,
        };
        db::insert(conn, &mem).expect("insert")
    }

    // Dry-run path: returns would_delete count without removing rows.
    #[test]
    fn forget_dry_run_counts_without_deleting() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "forget-ns", "a", Tier::Short);
        let _ = insert_one(&conn, "forget-ns", "b", Tier::Short);
        let resp = handle_forget(
            &conn,
            &json!({"namespace": "forget-ns", "dry_run": true}),
            false,
        )
        .expect("ok");
        assert_eq!(resp["dry_run"], true);
        assert_eq!(resp["would_delete"].as_u64(), Some(2));
        // Rows must still exist.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE namespace = 'forget-ns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    // Real-delete path: removes matching rows, returns deleted count.
    #[test]
    fn forget_deletes_matching_rows() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "del-ns", "a", Tier::Short);
        let _ = insert_one(&conn, "del-ns", "b", Tier::Short);
        let resp = handle_forget(
            &conn,
            &json!({"namespace": "del-ns", "dry_run": false}),
            false,
        )
        .expect("ok");
        assert_eq!(resp["deleted"].as_u64(), Some(2));
        assert_eq!(resp["archived"], false);
    }

    // Archive flag wired through verbatim.
    #[test]
    fn forget_with_archive_propagates_flag() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "arc-ns", "a", Tier::Mid);
        let resp = handle_forget(&conn, &json!({"namespace": "arc-ns"}), true).expect("ok");
        assert_eq!(resp["archived"], true);
        assert_eq!(resp["deleted"].as_u64(), Some(1));
    }

    // Tier filter is parsed and forwarded.
    #[test]
    fn forget_with_tier_filter() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "tier-ns", "s", Tier::Short);
        let _ = insert_one(&conn, "tier-ns", "m", Tier::Mid);
        let resp = handle_forget(
            &conn,
            &json!({"namespace": "tier-ns", "tier": "short"}),
            false,
        )
        .expect("ok");
        // Only the short-tier row should be deleted.
        assert_eq!(resp["deleted"].as_u64(), Some(1));
    }

    // Invalid tier string falls back to None (tier not applied).
    #[test]
    fn forget_with_invalid_tier_string_treated_as_none() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "bad-tier-ns", "x", Tier::Mid);
        let resp = handle_forget(
            &conn,
            &json!({"namespace": "bad-tier-ns", "tier": "not-a-tier", "dry_run": true}),
            false,
        )
        .expect("ok");
        assert_eq!(resp["would_delete"].as_u64(), Some(1));
    }

    // Substrate error path: no namespace, no pattern, no tier → error.
    #[test]
    fn forget_no_filter_returns_error() {
        let conn = fresh_conn();
        let err = handle_forget(&conn, &json!({"dry_run": true}), false).unwrap_err();
        assert!(!err.is_empty());
    }

    // handle_stats — returns a serializable stats object.
    #[test]
    fn stats_returns_object_shape() {
        let conn = fresh_conn();
        let _ = insert_one(&conn, "stats-ns", "a", Tier::Short);
        let resp = handle_stats(&conn, Path::new(":memory:")).expect("ok");
        assert!(resp.is_object(), "stats must be an object");
    }
}
