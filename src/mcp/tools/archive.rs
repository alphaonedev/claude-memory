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
