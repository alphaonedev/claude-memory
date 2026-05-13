// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Typed CRUD over the `governance_rules` table (migration
//! `0024_v07_governance_rules.sql`).
//!
//! The table holds the substrate-level agent-action rules consulted
//! by [`crate::governance::agent_action::check_agent_action`]. This
//! module owns the SQL — no other code path is allowed to SELECT /
//! INSERT / UPDATE / DELETE from `governance_rules` directly.
//!
//! The shape is deliberately small: five verbs ([`insert`],
//! [`get`], [`list`], [`list_enabled_by_kind`], [`remove`]) plus
//! two state mutators ([`set_enabled`], [`update_signature`]). All
//! verbs are idempotent against a missing row (`get` / `remove`
//! return `None` / `Ok(false)` rather than erroring).
//!
//! # Operator-mutation gating (NOT enforced here)
//!
//! The CRUD functions are pure SQL. The operator-keypair-on-disk
//! gating lives in `src/cli/rules.rs` and the HTTP handler — both
//! verify the signature header / file presence BEFORE calling these
//! verbs. The MCP read-only `rule_list` tool calls [`list`] /
//! [`get`]; mutation tools over MCP are explicitly disabled per
//! issue #691 design revision 2026-05-13.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// One row of `governance_rules`. Field order matches the SQL column
/// order so projection / debugging is symmetric.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub kind: String,
    pub matcher: String,
    pub severity: String,
    pub reason: String,
    pub namespace: String,
    pub created_by: String,
    pub created_at: i64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<Vec<u8>>,
    pub attest_level: String,
}

/// Insert a fresh rule. Returns an error if `id` already exists —
/// callers that want upsert semantics should call [`get`] first.
///
/// # Errors
///
/// Propagates SQLite errors. The `severity` value is enforced by the
/// table CHECK constraint (one of `refuse`/`warn`/`log`); a bad
/// value here will surface as a constraint error.
pub fn insert(conn: &Connection, rule: &Rule) -> Result<()> {
    conn.execute(
        "INSERT INTO governance_rules (
             id, kind, matcher, severity, reason, namespace,
             created_by, created_at, enabled, signature, attest_level
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            rule.id,
            rule.kind,
            rule.matcher,
            rule.severity,
            rule.reason,
            rule.namespace,
            rule.created_by,
            rule.created_at,
            i64::from(rule.enabled),
            rule.signature,
            rule.attest_level,
        ],
    )
    .with_context(|| format!("rules_store::insert: id={}", rule.id))?;
    Ok(())
}

/// Fetch a rule by id. Returns `None` when no row matches.
///
/// # Errors
///
/// Propagates SQLite errors. A row whose `enabled` column is not 0/1
/// or whose `severity` is outside the CHECK list will still
/// deserialize — the engine treats unknown severities as `Log`
/// (defensive), so the returned row is consumable.
pub fn get(conn: &Connection, id: &str) -> Result<Option<Rule>> {
    let row = conn
        .query_row(
            "SELECT id, kind, matcher, severity, reason, namespace,
                    created_by, created_at, enabled, signature, attest_level
             FROM governance_rules WHERE id = ?1",
            params![id],
            row_to_rule,
        )
        .optional()
        .with_context(|| format!("rules_store::get: id={id}"))?;
    Ok(row)
}

/// List every rule in the table (enabled and disabled). Ordered by
/// `id ASC` for deterministic CLI output.
///
/// # Errors
///
/// Propagates SQLite errors.
pub fn list(conn: &Connection) -> Result<Vec<Rule>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, matcher, severity, reason, namespace,
                    created_by, created_at, enabled, signature, attest_level
             FROM governance_rules ORDER BY id ASC",
        )
        .context("rules_store::list: prepare")?;
    let rows = stmt
        .query_map([], row_to_rule)
        .context("rules_store::list: query_map")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("rules_store::list: row")?);
    }
    Ok(out)
}

/// List only the rules of `kind` that are enabled. The dominant
/// query shape called once per [`crate::governance::agent_action::check_agent_action`]
/// invocation; covered by `idx_governance_rules_kind_enabled`.
///
/// Ordered by `id ASC` to make first-refusal-wins deterministic for
/// audit reproduction.
///
/// # Errors
///
/// Propagates SQLite errors.
pub fn list_enabled_by_kind(conn: &Connection, kind: &str) -> Result<Vec<Rule>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, kind, matcher, severity, reason, namespace,
                    created_by, created_at, enabled, signature, attest_level
             FROM governance_rules
             WHERE kind = ?1 AND enabled = 1
             ORDER BY id ASC",
        )
        .context("rules_store::list_enabled_by_kind: prepare")?;
    let rows = stmt
        .query_map(params![kind], row_to_rule)
        .context("rules_store::list_enabled_by_kind: query_map")?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.context("rules_store::list_enabled_by_kind: row")?);
    }
    Ok(out)
}

/// Remove a rule by id. Returns `true` when a row was deleted,
/// `false` when no row matched.
///
/// # Errors
///
/// Propagates SQLite errors.
pub fn remove(conn: &Connection, id: &str) -> Result<bool> {
    let affected = conn
        .execute("DELETE FROM governance_rules WHERE id = ?1", params![id])
        .with_context(|| format!("rules_store::remove: id={id}"))?;
    Ok(affected > 0)
}

/// Flip the `enabled` column on an existing rule. Returns `true`
/// when a row was updated, `false` when no row matched.
///
/// Used by `ai-memory rules enable` / `ai-memory rules disable` —
/// the CLI verifies the operator signature before calling here.
///
/// # Errors
///
/// Propagates SQLite errors.
pub fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<bool> {
    let affected = conn
        .execute(
            "UPDATE governance_rules SET enabled = ?1 WHERE id = ?2",
            params![i64::from(enabled), id],
        )
        .with_context(|| format!("rules_store::set_enabled: id={id} enabled={enabled}"))?;
    Ok(affected > 0)
}

/// Persist an operator signature + bump `attest_level` to
/// `operator_signed` on an existing rule. Used by `ai-memory rules
/// enable --sign` / `ai-memory rules add --sign` after the CLI
/// computes the Ed25519 signature over the canonical row encoding.
///
/// # Errors
///
/// Propagates SQLite errors. Returns `false` when no row matched.
pub fn update_signature(
    conn: &Connection,
    id: &str,
    signature: &[u8],
    attest_level: &str,
) -> Result<bool> {
    let affected = conn
        .execute(
            "UPDATE governance_rules
             SET signature = ?1, attest_level = ?2
             WHERE id = ?3",
            params![signature, attest_level, id],
        )
        .with_context(|| format!("rules_store::update_signature: id={id}"))?;
    Ok(affected > 0)
}

/// Canonical byte encoding of a rule for signature input. Stable
/// across versions: the field order is fixed; the wire format is
/// `serde_json` compact (no whitespace). A future signature-format
/// migration recomputes the bytes via a different canonicalizer
/// without touching the CLI call sites.
///
/// # Errors
///
/// Propagates `serde_json` encoding errors.
pub fn canonical_bytes(rule: &Rule) -> Result<Vec<u8>> {
    let canonical = serde_json::json!({
        "id": rule.id,
        "kind": rule.kind,
        "matcher": rule.matcher,
        "severity": rule.severity,
        "reason": rule.reason,
        "namespace": rule.namespace,
        "created_by": rule.created_by,
        "created_at": rule.created_at,
    });
    serde_json::to_vec(&canonical).context("rules_store::canonical_bytes: serialize")
}

fn row_to_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get(0)?,
        kind: row.get(1)?,
        matcher: row.get(2)?,
        severity: row.get(3)?,
        reason: row.get(4)?,
        namespace: row.get(5)?,
        created_by: row.get(6)?,
        created_at: row.get(7)?,
        enabled: row.get::<_, i64>(8)? != 0,
        signature: row.get(9)?,
        attest_level: row.get(10)?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE governance_rules (
                 id TEXT PRIMARY KEY,
                 kind TEXT NOT NULL,
                 matcher TEXT NOT NULL,
                 severity TEXT NOT NULL CHECK (severity IN ('refuse','warn','log')),
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

    fn make_rule(id: &str, kind: &str, enabled: bool) -> Rule {
        Rule {
            id: id.to_string(),
            kind: kind.to_string(),
            matcher: r#"{"k":"v"}"#.to_string(),
            severity: "refuse".to_string(),
            reason: "test".to_string(),
            namespace: "_global".to_string(),
            created_by: "test".to_string(),
            created_at: 12345,
            enabled,
            signature: None,
            attest_level: "unsigned".to_string(),
        }
    }

    #[test]
    fn insert_then_get_roundtrip() {
        let conn = fresh_conn();
        let rule = make_rule("R1", "bash", true);
        insert(&conn, &rule).unwrap();
        let got = get(&conn, "R1").unwrap();
        assert_eq!(got.as_ref(), Some(&rule));
    }

    #[test]
    fn get_returns_none_when_missing() {
        let conn = fresh_conn();
        assert_eq!(get(&conn, "nope").unwrap(), None);
    }

    #[test]
    fn insert_duplicate_id_errors() {
        let conn = fresh_conn();
        let rule = make_rule("R1", "bash", true);
        insert(&conn, &rule).unwrap();
        assert!(insert(&conn, &rule).is_err());
    }

    #[test]
    fn list_orders_by_id_ascending() {
        let conn = fresh_conn();
        insert(&conn, &make_rule("R3", "bash", true)).unwrap();
        insert(&conn, &make_rule("R1", "bash", true)).unwrap();
        insert(&conn, &make_rule("R2", "bash", true)).unwrap();
        let all = list(&conn).unwrap();
        let ids: Vec<&str> = all.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["R1", "R2", "R3"]);
    }

    #[test]
    fn list_enabled_by_kind_filters_correctly() {
        let conn = fresh_conn();
        insert(&conn, &make_rule("R1", "bash", true)).unwrap();
        insert(&conn, &make_rule("R2", "bash", false)).unwrap();
        insert(&conn, &make_rule("R3", "filesystem_write", true)).unwrap();
        let bash_rules = list_enabled_by_kind(&conn, "bash").unwrap();
        assert_eq!(bash_rules.len(), 1);
        assert_eq!(bash_rules[0].id, "R1");
        let fs_rules = list_enabled_by_kind(&conn, "filesystem_write").unwrap();
        assert_eq!(fs_rules.len(), 1);
        assert_eq!(fs_rules[0].id, "R3");
        let other = list_enabled_by_kind(&conn, "network_request").unwrap();
        assert!(other.is_empty());
    }

    #[test]
    fn remove_returns_true_on_hit_false_on_miss() {
        let conn = fresh_conn();
        insert(&conn, &make_rule("R1", "bash", true)).unwrap();
        assert!(remove(&conn, "R1").unwrap());
        assert!(!remove(&conn, "R1").unwrap());
        assert_eq!(get(&conn, "R1").unwrap(), None);
    }

    #[test]
    fn set_enabled_toggles() {
        let conn = fresh_conn();
        insert(&conn, &make_rule("R1", "bash", false)).unwrap();
        assert!(set_enabled(&conn, "R1", true).unwrap());
        assert!(get(&conn, "R1").unwrap().unwrap().enabled);
        assert!(set_enabled(&conn, "R1", false).unwrap());
        assert!(!get(&conn, "R1").unwrap().unwrap().enabled);
    }

    #[test]
    fn set_enabled_missing_returns_false() {
        let conn = fresh_conn();
        assert!(!set_enabled(&conn, "nope", true).unwrap());
    }

    #[test]
    fn update_signature_persists_blob_and_attest_level() {
        let conn = fresh_conn();
        insert(&conn, &make_rule("R1", "bash", true)).unwrap();
        let sig = vec![1u8, 2, 3, 4];
        assert!(update_signature(&conn, "R1", &sig, "operator_signed").unwrap());
        let got = get(&conn, "R1").unwrap().unwrap();
        assert_eq!(got.signature, Some(sig));
        assert_eq!(got.attest_level, "operator_signed");
    }

    #[test]
    fn update_signature_missing_returns_false() {
        let conn = fresh_conn();
        assert!(!update_signature(&conn, "nope", &[1, 2, 3], "operator_signed").unwrap());
    }

    #[test]
    fn canonical_bytes_excludes_signature_fields() {
        let mut rule = make_rule("R1", "bash", true);
        rule.signature = Some(vec![9, 9, 9]);
        rule.attest_level = "operator_signed".to_string();
        let bytes = canonical_bytes(&rule).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // The signature itself must NOT appear in the canonical
        // input (otherwise we'd be signing the signature).
        assert!(!s.contains("signature"));
        assert!(!s.contains("attest_level"));
        assert!(s.contains("\"id\":\"R1\""));
        assert!(s.contains("\"kind\":\"bash\""));
    }

    #[test]
    fn severity_check_constraint_rejects_unknown() {
        let conn = fresh_conn();
        let mut rule = make_rule("R1", "bash", true);
        rule.severity = "unknown".to_string();
        assert!(insert(&conn, &rule).is_err());
    }

    #[test]
    fn rule_serde_roundtrip() {
        let rule = make_rule("R1", "bash", true);
        let v = serde_json::to_value(&rule).unwrap();
        let back: Rule = serde_json::from_value(v).unwrap();
        assert_eq!(back, rule);
    }
}
