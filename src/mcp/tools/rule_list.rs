// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! MCP `memory_rule_list` handler (issue #691).
//!
//! Read-only listing of the substrate's `governance_rules` table.
//! Accepts an optional `kind` filter and an optional `enabled_only`
//! flag; default returns every row sorted by id ASC.
//!
//! # MCP mutation is disabled
//!
//! Per issue #691 design revision 2026-05-13, MCP stdio cannot
//! mutate rules. Use the CLI (`ai-memory rules add --sign`) or the
//! HTTP admin endpoints (`POST /api/v1/governance/rules` with the
//! `X-AI-Memory-Operator-Signature` header).

use base64::Engine;
use serde_json::{Value, json};

use crate::governance::rules_store::{self, Rule};

/// Handler for `memory_rule_list`. Accepts:
///
/// ```json
/// {
///   "kind": "filesystem_write" (optional),
///   "enabled_only": true (optional, defaults to false)
/// }
/// ```
///
/// Returns:
///
/// ```json
/// {
///   "count": <n>,
///   "rules": [ { id, kind, matcher, severity, reason, ... }, ... ]
/// }
/// ```
pub fn handle_rule_list(conn: &rusqlite::Connection, arguments: &Value) -> Result<Value, String> {
    let kind_filter = arguments.get("kind").and_then(Value::as_str);
    let enabled_only = arguments
        .get("enabled_only")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let rules: Vec<Rule> = if let Some(kind) = kind_filter {
        if enabled_only {
            rules_store::list_enabled_by_kind(conn, kind).map_err(|e| e.to_string())?
        } else {
            // No "list_by_kind" helper today — we filter in-memory
            // from `list` to keep the store surface small. The
            // governance_rules table is operator-scale (typical
            // deployment <100 rows) so the scan is fine.
            rules_store::list(conn)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|r| r.kind == kind)
                .collect()
        }
    } else if enabled_only {
        rules_store::list(conn)
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|r| r.enabled)
            .collect()
    } else {
        rules_store::list(conn).map_err(|e| e.to_string())?
    };

    let mut out = Vec::with_capacity(rules.len());
    for r in &rules {
        let sig_b64 = r
            .signature
            .as_ref()
            .map(|b| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b));
        out.push(json!({
            "id": r.id,
            "kind": r.kind,
            "matcher": r.matcher,
            "severity": r.severity,
            "reason": r.reason,
            "namespace": r.namespace,
            "created_by": r.created_by,
            "created_at": r.created_at,
            "enabled": r.enabled,
            "signature_b64": sig_b64,
            "attest_level": r.attest_level,
        }));
    }
    Ok(json!({
        "count": out.len(),
        "rules": out,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 namespace TEXT NOT NULL DEFAULT '_global',
                 created_by TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 enabled INTEGER NOT NULL DEFAULT 1,
                 signature BLOB,
                 attest_level TEXT NOT NULL DEFAULT 'unsigned'
             );",
        )
        .unwrap();
        conn
    }

    fn insert(conn: &rusqlite::Connection, id: &str, kind: &str, enabled: bool) {
        rules_store::insert(
            conn,
            &Rule {
                id: id.into(),
                kind: kind.into(),
                matcher: r#"{"k":"v"}"#.into(),
                severity: "refuse".into(),
                reason: "r".into(),
                namespace: "_global".into(),
                created_by: "test".into(),
                created_at: 0,
                enabled,
                signature: None,
                attest_level: "unsigned".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn empty_returns_zero() {
        let conn = fresh_conn();
        let r = handle_rule_list(&conn, &json!({})).unwrap();
        assert_eq!(r["count"], 0);
    }

    #[test]
    fn lists_all_rules_by_default() {
        let conn = fresh_conn();
        insert(&conn, "R1", "bash", true);
        insert(&conn, "R2", "filesystem_write", false);
        let r = handle_rule_list(&conn, &json!({})).unwrap();
        assert_eq!(r["count"], 2);
    }

    #[test]
    fn filters_by_kind() {
        let conn = fresh_conn();
        insert(&conn, "R1", "bash", true);
        insert(&conn, "R2", "filesystem_write", true);
        let r = handle_rule_list(&conn, &json!({"kind":"bash"})).unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["rules"][0]["id"], "R1");
    }

    #[test]
    fn enabled_only_skips_disabled() {
        let conn = fresh_conn();
        insert(&conn, "R1", "bash", true);
        insert(&conn, "R2", "bash", false);
        let r = handle_rule_list(&conn, &json!({"enabled_only":true})).unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["rules"][0]["id"], "R1");
    }

    #[test]
    fn kind_and_enabled_only_combined() {
        // Issue #819 — handle_rule_list internally uses
        // list_enabled_by_kind which filters by operator pubkey signature.
        // Suppress pubkey resolution so the unsigned R1/R3 fixtures
        // surface regardless of dev-host / CI-runner state.
        let _no_pubkey = crate::governance::rules_store::force_no_operator_pubkey_for_test();
        let conn = fresh_conn();
        insert(&conn, "R1", "bash", true);
        insert(&conn, "R2", "bash", false);
        insert(&conn, "R3", "filesystem_write", true);
        let r = handle_rule_list(&conn, &json!({"kind":"bash","enabled_only":true})).unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["rules"][0]["id"], "R1");
    }
}
