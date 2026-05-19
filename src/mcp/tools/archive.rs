// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP archive management handlers (list, restore, purge, stats, gc).

use crate::db;
use serde_json::{Value, json};
pub(super) fn handle_archive_list(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let namespace = params["namespace"].as_str();
    let limit = usize::try_from(params["limit"].as_u64().unwrap_or(50)).unwrap_or(usize::MAX);
    let offset = usize::try_from(params["offset"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX);
    let items =
        db::list_archived(conn, namespace, limit.min(1000), offset).map_err(|e| e.to_string())?;
    Ok(json!({"archived": items, "count": items.len()}))
}

pub(super) fn handle_archive_restore(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let id = params["id"].as_str().ok_or("id is required")?;
    crate::validate::validate_id(id).map_err(|e| e.to_string())?;
    let restored = db::restore_archived(conn, id).map_err(|e| e.to_string())?;
    if !restored {
        return Err("not found in archive".into());
    }
    Ok(json!({"restored": true, "id": id}))
}

pub(super) fn handle_archive_purge(
    conn: &rusqlite::Connection,
    params: &Value,
) -> Result<Value, String> {
    let older_than_days = params["older_than_days"].as_i64();

    // #913 (security-medium / SOC2, 2026-05-19) — admin/destructive
    // state-change audit. Archive purge permanently deletes archived
    // memories; emit the forensic-chain row BEFORE the storage write
    // so the audit trail captures intent regardless of downstream
    // permission-gate / storage outcome. Mirrors the #911 HTTP
    // `purge_archive` fix.
    let caller = crate::identity::resolve_agent_id(params["agent_id"].as_str(), None)
        .unwrap_or_else(|_| "anonymous:invalid".to_string());
    crate::governance::audit::record_decision(
        &caller,
        "allow",
        "archive_purge",
        "",
        json!({ "older_than_days": older_than_days }),
    );

    // v0.7.0 K9 — unified permission pipeline (archive-side).
    // Archive purge is a destructive across-namespace operation; we
    // evaluate against the global namespace + caller's agent_id.
    // Operators can still scope rules via `namespace_pattern = "**"`.
    {
        use crate::permissions::{Op, PermissionContext, Permissions};
        let agent_id = crate::identity::resolve_agent_id(params["agent_id"].as_str(), None)
            .map_err(|e| e.to_string())?;
        let ctx = PermissionContext {
            op: Op::MemoryArchive,
            namespace: "global".to_string(),
            agent_id,
            payload: json!({"older_than_days": older_than_days}),
        };
        match Permissions::evaluate(&ctx, &[]) {
            crate::permissions::Decision::Allow | crate::permissions::Decision::Modify(_) => {}
            crate::permissions::Decision::Deny(reason) => {
                return Err(format!("archive denied by permission rule: {reason}"));
            }
            crate::permissions::Decision::Ask(prompt) => {
                return Ok(json!({
                    "status": "ask",
                    "reason": prompt,
                    "action": "archive",
                }));
            }
        }
    }

    let purged = db::purge_archive(conn, older_than_days).map_err(|e| e.to_string())?;
    Ok(json!({"purged": purged}))
}

pub(super) fn handle_archive_stats(conn: &rusqlite::Connection) -> Result<Value, String> {
    db::archive_stats(conn).map_err(|e| e.to_string())
}

pub(super) fn handle_gc(
    conn: &rusqlite::Connection,
    params: &Value,
    archive: bool,
) -> Result<Value, String> {
    let dry_run = params["dry_run"].as_bool().unwrap_or(false);
    if dry_run {
        // Just count expired without deleting
        let now = chrono::Utc::now().to_rfc3339();
        let count: usize = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE expires_at IS NOT NULL AND expires_at < ?1",
                rusqlite::params![now],
                |r| r.get(0),
            )
            .unwrap_or(0);
        return Ok(json!({"collected": count, "dry_run": true}));
    }
    let count = db::gc(conn, archive).map_err(|e| e.to_string())?;
    Ok(json!({"collected": count, "dry_run": false}))
}

// ---- C-5 (#699): unit coverage for the `pub(super)` handlers. The MCP
// dispatch layer covers most happy paths; these target the missing-`id`,
// invalid-id and "not in archive" branches plus the gc dry-run vs.
// actual-run split that the lib-tier path under-exercises (currently
// 91.02%). ----
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn open_conn() -> rusqlite::Connection {
        crate::db::open(std::path::Path::new(":memory:")).expect("open in-memory db")
    }

    #[test]
    fn handle_archive_restore_missing_id_errors() {
        // Hits the `id is required` branch on line 24.
        let conn = open_conn();
        let err = handle_archive_restore(&conn, &json!({})).unwrap_err();
        assert!(err.contains("id"), "got: {err}");
    }

    #[test]
    fn handle_archive_restore_invalid_id_maps_validator_error() {
        // Covers `validate_id(...).map_err(...)` on line 25.
        let conn = open_conn();
        let err = handle_archive_restore(&conn, &json!({"id": "not-a-valid-uuid"})).unwrap_err();
        assert!(!err.is_empty(), "expected non-empty validator error");
    }

    #[test]
    fn handle_archive_restore_unknown_uuid_returns_not_found() {
        // Well-formed UUID but no row exists → line 28 "not found in archive".
        let conn = open_conn();
        let err = handle_archive_restore(
            &conn,
            &json!({"id": "00000000-0000-0000-0000-000000000000"}),
        )
        .unwrap_err();
        assert!(err.contains("not found"), "got: {err}");
    }

    #[test]
    fn handle_archive_list_default_paging_returns_empty() {
        // Exercises `params["limit"].as_u64().unwrap_or(50)` and
        // `params["offset"].as_u64().unwrap_or(0)` defaults on lines 13-14.
        let conn = open_conn();
        let result = handle_archive_list(&conn, &json!({})).expect("list ok");
        assert_eq!(result["count"], 0);
        assert!(result["archived"].is_array());
    }

    #[test]
    fn handle_archive_stats_returns_object() {
        // Covers the `archive_stats(...).map_err(...)` happy path
        // (line 73) on an empty DB. The stats schema is an object.
        let conn = open_conn();
        let result = handle_archive_stats(&conn).expect("stats ok");
        assert!(
            result.is_object(),
            "archive_stats must return a JSON object on empty DB, got: {result}"
        );
    }

    #[test]
    fn handle_gc_dry_run_on_empty_db_returns_zero() {
        // Covers the `dry_run = true` branch on lines 82-92.
        let conn = open_conn();
        let result = handle_gc(&conn, &json!({"dry_run": true}), false).expect("gc dry-run ok");
        assert_eq!(result["collected"], 0);
        assert_eq!(result["dry_run"], true);
    }

    #[test]
    fn handle_gc_actual_run_on_empty_db_returns_zero() {
        // Covers the actual-gc branch on lines 94-95 with archive=true.
        let conn = open_conn();
        let result = handle_gc(&conn, &json!({"dry_run": false}), true).expect("gc run ok");
        assert_eq!(result["collected"], 0);
        assert_eq!(result["dry_run"], false);
    }

    #[test]
    fn handle_archive_purge_default_no_filter_succeeds_on_empty_db() {
        // Covers the `older_than_days` None path on line 37, and the
        // permission-Allow happy path (lines 53-54), and the
        // `purge_archive(...)` success branch on lines 68-69.
        let conn = open_conn();
        let result = handle_archive_purge(&conn, &json!({})).expect("purge ok");
        let purged = &result["purged"];
        // Single-branch numeric assertion so the `||` short-circuit
        // doesn't leave the right side unexercised.
        assert!(
            purged.is_number(),
            "expected numeric `purged`, got: {purged}"
        );
    }
}
