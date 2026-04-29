// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! Shared governance enforcement helper. Wave 5b (v0.6.3) lifted the
//! `match db::enforce_governance(...)` block out of every governed
//! `cmd_*` so the printing-side of governance decisions has a single
//! testable home and the call-sites collapse to a 3-arm match on the
//! returned [`GovernanceOutcome`].
//!
//! ## Why a separate module
//!
//! Each governed command (`store`, `delete`, `promote`) used to repeat
//! the same 25-line block:
//!
//! ```ignore
//! match db::enforce_governance(...)? {
//!     Allow => {}
//!     Deny(r) => { eprintln!(...); std::process::exit(1); }
//!     Pending(id) => { /* print + return */ }
//! }
//! ```
//!
//! That made the printing format (text vs JSON, the literal field names)
//! invisible to unit tests because they couldn't run a process-exit
//! branch in-process. Lifting it here lets us:
//!
//! 1. Test the **printing side** of Pending and Deny without crashing
//!    the test runner (the helper writes the message and returns; the
//!    caller decides whether to exit).
//! 2. Keep one canonical JSON shape for `pending_actions` responses.
//!
//! ## Public surface
//!
//! ```ignore
//! pub enum GovernanceOutcome { Allow, Pending, Deny }
//!
//! pub fn enforce(
//!     conn: &Connection,
//!     action: GovernedAction,
//!     namespace: &str,
//!     caller_agent_id: &str,
//!     memory_id: Option<&str>,
//!     memory_owner: Option<&str>,
//!     payload: &serde_json::Value,
//!     json_out: bool,
//!     out: &mut CliOutput<'_>,
//! ) -> Result<GovernanceOutcome>;
//! ```
//!
//! - `Allow`: silent, caller proceeds.
//! - `Pending`: helper writes a `pending_actions` record (text or JSON
//!   shape, `out.stdout`) and returns `Pending`. Caller usually returns
//!   `Ok(())` immediately.
//! - `Deny`: helper writes the deny reason to `out.stderr` and returns
//!   `Deny`. Caller is expected to `std::process::exit(1)` after the
//!   helper returns — exiting stays inline so this module is testable.

use crate::cli::CliOutput;
use crate::{db, models};
use anyhow::Result;
use models::{GovernanceDecision, GovernedAction};
use rusqlite::Connection;

/// Outcome surfaced to the caller. Mirrors [`GovernanceDecision`] but
/// erases the inner strings — the helper has already printed them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GovernanceOutcome {
    /// Allow; caller proceeds with the action.
    Allow,
    /// Pending; helper printed the queued-for-approval message. Caller
    /// usually returns `Ok(())` immediately.
    Pending,
    /// Deny; helper printed the reason to stderr. Caller is expected to
    /// exit non-zero.
    Deny,
}

/// Run `db::enforce_governance` and route the print-side of Pending/Deny
/// through `out`. Returns a [`GovernanceOutcome`] so the caller can
/// decide whether to continue, return, or exit.
///
/// Does **not** call `std::process::exit` on Deny — the exit stays at
/// the call-site so this module is testable in-process.
#[allow(clippy::too_many_arguments)]
pub fn enforce(
    conn: &Connection,
    action: GovernedAction,
    namespace: &str,
    caller_agent_id: &str,
    memory_id: Option<&str>,
    memory_owner: Option<&str>,
    payload: &serde_json::Value,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<GovernanceOutcome> {
    match db::enforce_governance(
        conn,
        action,
        namespace,
        caller_agent_id,
        memory_id,
        memory_owner,
        payload,
    )? {
        GovernanceDecision::Allow => Ok(GovernanceOutcome::Allow),
        GovernanceDecision::Deny(reason) => {
            writeln!(
                out.stderr,
                "{} denied by governance: {reason}",
                action.as_str()
            )?;
            Ok(GovernanceOutcome::Deny)
        }
        GovernanceDecision::Pending(pending_id) => {
            if json_out {
                let mut payload_obj = serde_json::json!({
                    "status": "pending",
                    "pending_id": pending_id,
                    "reason": "governance requires approval",
                    "action": action.as_str(),
                    "namespace": namespace,
                });
                if let Some(mid) = memory_id
                    && let Some(obj) = payload_obj.as_object_mut()
                {
                    obj.insert(
                        "memory_id".to_string(),
                        serde_json::Value::String(mid.to_string()),
                    );
                }
                writeln!(out.stdout, "{payload_obj}")?;
            } else if let Some(mid) = memory_id {
                writeln!(
                    out.stdout,
                    "{} queued for approval: pending_id={pending_id} id={mid}",
                    action.as_str()
                )?;
            } else {
                writeln!(
                    out.stdout,
                    "{} queued for approval: pending_id={pending_id} ns={namespace}",
                    action.as_str()
                )?;
            }
            Ok(GovernanceOutcome::Pending)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};
    use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy};

    /// Seed a namespace standard with the supplied governance policy. The
    /// standard memory is inserted in `_standards` and pinned via
    /// `set_namespace_standard`.
    fn seed_governance_policy(
        db_path: &std::path::Path,
        namespace: &str,
        policy: GovernancePolicy,
        owner_agent_id: &str,
    ) {
        let conn = db::open(db_path).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut metadata = models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String(owner_agent_id.to_string()),
            );
            obj.insert(
                "governance".to_string(),
                serde_json::to_value(&policy).unwrap(),
            );
        }
        let standard = models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: models::Tier::Long,
            namespace: format!("_standards-{namespace}"),
            title: format!("standard for {namespace}"),
            content: "policy".to_string(),
            tags: vec![],
            priority: 9,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
        };
        let standard_id = db::insert(&conn, &standard).unwrap();
        db::set_namespace_standard(&conn, namespace, &standard_id, None).unwrap();
    }

    #[test]
    fn test_governance_allow_returns_allow_no_output() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        // Touch DB to materialize schema
        let _ = seed_memory(&db_path, "ns", "x", "y");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({});
        let outcome = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Store,
                "ns-without-policy",
                "alice",
                None,
                None,
                &payload,
                false,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Allow);
        assert!(env.stdout_str().is_empty());
        assert!(env.stderr_str().is_empty());
    }

    #[test]
    fn test_governance_pending_writes_pending_status_text() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let policy = GovernancePolicy {
            write: GovernanceLevel::Approve,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        };
        seed_governance_policy(&db_path, "gov-ns", policy, "alice");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({"title": "t"});
        let outcome = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Store,
                "gov-ns",
                "bob",
                None,
                None,
                &payload,
                false,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Pending);
        let stdout = env.stdout_str();
        assert!(stdout.contains("queued for approval"), "got: {stdout}");
        assert!(stdout.contains("pending_id="), "got: {stdout}");
        assert!(stdout.contains("ns=gov-ns"), "got: {stdout}");
    }

    #[test]
    fn test_governance_pending_writes_pending_status_json() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Approve,
            approver: ApproverType::Human,
        };
        seed_governance_policy(&db_path, "gov-ns", policy, "alice");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({});
        let outcome = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Delete,
                "gov-ns",
                "bob",
                Some("00000000-0000-0000-0000-000000000abc"),
                Some("alice"),
                &payload,
                true,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Pending);
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["status"].as_str().unwrap(), "pending");
        assert_eq!(v["action"].as_str().unwrap(), "delete");
        assert_eq!(v["namespace"].as_str().unwrap(), "gov-ns");
        assert!(v["pending_id"].is_string());
        assert_eq!(
            v["memory_id"].as_str().unwrap(),
            "00000000-0000-0000-0000-000000000abc"
        );
    }

    #[test]
    fn test_governance_deny_writes_reason_to_stderr() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        };
        seed_governance_policy(&db_path, "gov-ns", policy, "alice");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({});
        let outcome = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Delete,
                "gov-ns",
                "bob",
                Some("00000000-0000-0000-0000-000000000def"),
                Some("alice"),
                &payload,
                false,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Deny);
        let stderr = env.stderr_str();
        assert!(
            stderr.contains("delete denied by governance"),
            "got: {stderr}"
        );
        assert!(stderr.contains("not the owner"), "got: {stderr}");
        // No stdout for Deny.
        assert!(env.stdout_str().is_empty());
    }

    #[test]
    fn test_governance_deny_returns_deny_outcome() {
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let policy = GovernancePolicy {
            write: GovernanceLevel::Registered,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        };
        seed_governance_policy(&db_path, "gov-ns", policy, "alice");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({});
        let outcome = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Store,
                "gov-ns",
                "unregistered-caller",
                None,
                None,
                &payload,
                false,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Deny);
        assert!(env.stderr_str().contains("not a registered agent"));
    }

    #[test]
    fn test_governance_payload_serializes_correctly() {
        // The payload arg is forwarded into queue_pending_action so a
        // peer-side approver can replay the original request. Sanity
        // check: the exact bytes we passed in are stored in the
        // pending_actions row.
        let mut env = TestEnv::fresh();
        let db_path = env.db_path.clone();
        let policy = GovernancePolicy {
            write: GovernanceLevel::Approve,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        };
        seed_governance_policy(&db_path, "gov-ns", policy, "alice");
        let conn = db::open(&db_path).unwrap();
        let payload = serde_json::json!({"title": "hello", "priority": 7});
        let _ = {
            let mut out = env.output();
            enforce(
                &conn,
                GovernedAction::Store,
                "gov-ns",
                "carol",
                None,
                None,
                &payload,
                true,
                &mut out,
            )
            .unwrap()
        };
        // Locate the row we just queued and verify the payload JSON
        // round-trips byte-for-byte (modulo serialization order).
        let stored_payload: String = conn
            .query_row(
                "SELECT payload FROM pending_actions WHERE namespace = 'gov-ns' AND requested_by = 'carol' ORDER BY requested_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored_payload).unwrap();
        assert_eq!(v["title"].as_str().unwrap(), "hello");
        assert_eq!(v["priority"].as_u64().unwrap(), 7);
    }
}
