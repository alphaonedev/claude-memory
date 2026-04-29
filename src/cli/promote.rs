// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_promote` migration. See `cli::store` for the design pattern.
//!
//! ## Two axes of promotion
//!
//! - **Horizontal (default):** bump the memory's tier to `long`. Sets
//!   `expires_at = ""` to clear the inherited tier-default TTL.
//! - **Vertical (`--to-namespace`):** clone the memory into an ancestor
//!   namespace; the original is untouched, the tier is preserved.

use crate::cli::CliOutput;
use crate::cli::governance::{GovernanceOutcome, enforce as enforce_governance};
use crate::cli::helpers::id_short;
use crate::{db, identity, models, validate};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;

#[derive(Args)]
pub struct PromoteArgs {
    pub id: String,
    /// Task 1.7: clone this memory into a hierarchical-ancestor namespace
    /// (the original is untouched). Must be an ancestor of the memory's
    /// current namespace. Skips the tier bump — vertical promotion is a
    /// separate axis from tier promotion.
    #[arg(long)]
    pub to_namespace: Option<String>,
}

/// `promote` handler.
#[allow(clippy::too_many_lines)]
pub fn cmd_promote(
    db_path: &Path,
    args: &PromoteArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    validate::validate_id(&args.id)?;
    if let Some(ref to_ns) = args.to_namespace {
        validate::validate_namespace(to_ns)?;
    }
    let conn = db::open(db_path)?;
    let target = if let Some(m) = db::get(&conn, &args.id)? {
        m
    } else if let Some(m) = db::get_by_prefix(&conn, &args.id)? {
        m
    } else {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    };
    let resolved_id = target.id.clone();

    {
        use models::GovernedAction;
        let caller_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = serde_json::json!({
            "id": resolved_id,
            "to_namespace": args.to_namespace,
        });
        match enforce_governance(
            &conn,
            GovernedAction::Promote,
            &target.namespace,
            &caller_agent_id,
            Some(&resolved_id),
            mem_owner.as_deref(),
            &payload,
            json_out,
            out,
        )? {
            GovernanceOutcome::Allow => {}
            GovernanceOutcome::Deny => {
                std::process::exit(1);
            }
            GovernanceOutcome::Pending => {
                return Ok(());
            }
        }
    }

    if let Some(ref to_ns) = args.to_namespace {
        let clone_id = db::promote_to_namespace(&conn, &resolved_id, to_ns)?;
        if json_out {
            writeln!(
                out.stdout,
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "promoted": true,
                    "mode": "vertical",
                    "source_id": resolved_id,
                    "clone_id": clone_id,
                    "to_namespace": to_ns,
                }))?
            )?;
        } else {
            writeln!(
                out.stdout,
                "promoted (vertical): {} → {} (clone: {})",
                id_short(&resolved_id),
                to_ns,
                id_short(&clone_id),
            )?;
        }
        return Ok(());
    }

    let (found, _) = db::update(
        &conn,
        &resolved_id,
        None,
        None,
        Some(&Tier::Long),
        None,
        None,
        None,
        None,
        Some(""),
        None,
    )?;
    if !found {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    }
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({"promoted": true, "id": resolved_id, "tier": "long"})
        )?;
    } else {
        writeln!(out.stdout, "promoted to long-term: {resolved_id}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn promote_args(id: &str) -> PromoteArgs {
        PromoteArgs {
            id: id.to_string(),
            to_namespace: None,
        }
    }

    fn seed_governance_policy(
        db_path: &Path,
        namespace: &str,
        promote_level: models::GovernanceLevel,
        owner_agent_id: &str,
    ) {
        use models::{ApproverType, GovernanceLevel, GovernancePolicy};
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: promote_level,
            delete: GovernanceLevel::Owner,
            approver: ApproverType::Human,
        };
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
            tier: Tier::Long,
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
    fn test_promote_horizontal_to_long() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let args = promote_args(&id);
        {
            let mut out = env.output();
            cmd_promote(&db, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["promoted"].as_bool().unwrap(), true);
        assert_eq!(v["tier"].as_str().unwrap(), "long");
        let conn = db::open(&db).unwrap();
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(mem.tier, Tier::Long);
    }

    #[test]
    fn test_promote_by_prefix() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let prefix = id[..8].to_string();
        let args = promote_args(&prefix);
        {
            let mut out = env.output();
            cmd_promote(&db, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["id"].as_str().unwrap(), id);
    }

    #[test]
    fn test_promote_vertical_with_to_namespace() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Hierarchical namespaces use `/`. The memory in `parent/child`
        // can be promoted to ancestor `parent`.
        let id = seed_memory(&db, "parent/child", "tt", "cc");
        let mut args = promote_args(&id);
        args.to_namespace = Some("parent".to_string());
        {
            let mut out = env.output();
            cmd_promote(&db, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["mode"].as_str().unwrap(), "vertical");
        assert!(v["clone_id"].is_string());
        assert_eq!(v["to_namespace"].as_str().unwrap(), "parent");
    }

    #[test]
    fn test_promote_vertical_invalid_namespace_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let mut args = promote_args(&id);
        args.to_namespace = Some("has spaces".to_string());
        let mut out = env.output();
        let res = cmd_promote(&db, &args, false, Some("test-agent"), &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_promote_governance_pending() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "gov-promote-ns", "tt", "cc");
        seed_governance_policy(
            &db,
            "gov-promote-ns",
            models::GovernanceLevel::Approve,
            "alice",
        );
        let args = promote_args(&id);
        {
            let mut out = env.output();
            cmd_promote(&db, &args, true, Some("bob"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["status"].as_str().unwrap(), "pending");
        assert_eq!(v["action"].as_str().unwrap(), "promote");
        // Memory must NOT be promoted on Pending — tier still mid.
        let conn = db::open(&db).unwrap();
        let mem = db::get(&conn, &id).unwrap().unwrap();
        assert_eq!(mem.tier, Tier::Mid);
    }

    #[test]
    fn test_promote_governance_deny() {
        // The Deny branch in cmd_promote calls std::process::exit, which
        // tears down the test runner. The print-side of Deny is covered
        // by `cli::governance::tests::test_governance_deny_writes_reason_to_stderr`.
        // Here we exercise the helper directly with a Promote action against
        // an Owner-gated namespace and confirm the GovernanceOutcome::Deny
        // wiring + the literal stderr line cmd_promote would print.
        use crate::cli::governance::{GovernanceOutcome, enforce as enforce_governance};
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let conn = db::open(&db).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut metadata = models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String("alice".to_string()),
            );
        }
        let mem = models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Mid,
            namespace: "deny-ns".to_string(),
            title: "tt".to_string(),
            content: "cc".to_string(),
            tags: vec![],
            priority: 5,
            confidence: 1.0,
            source: "test".to_string(),
            access_count: 0,
            created_at: now.clone(),
            updated_at: now,
            last_accessed_at: None,
            expires_at: None,
            metadata,
        };
        let id = db::insert(&conn, &mem).unwrap();
        drop(conn);
        seed_governance_policy(&db, "deny-ns", models::GovernanceLevel::Owner, "alice");

        let conn = db::open(&db).unwrap();
        let payload = serde_json::json!({"id": id, "to_namespace": serde_json::Value::Null});
        let outcome = {
            let mut out = env.output();
            enforce_governance(
                &conn,
                models::GovernedAction::Promote,
                "deny-ns",
                "bob",
                Some(&id),
                Some("alice"),
                &payload,
                false,
                &mut out,
            )
            .unwrap()
        };
        assert_eq!(outcome, GovernanceOutcome::Deny);
        assert!(env.stderr_str().contains("promote denied by governance"));
    }

    // Nonexistent id triggers process::exit; covered by the integration
    // suite that spawns the binary. In-process the validate_id branch
    // proxies the not-found case for malformed inputs.
    #[test]
    fn test_promote_nonexistent_exits_nonzero() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Malformed id with a null byte hits validate_id before the
        // not-found exit branch — keeps the test in-process.
        let bad = "bad\0id".to_string();
        let args = promote_args(&bad);
        let mut out = env.output();
        let res = cmd_promote(&db, &args, false, Some("x"), &mut out);
        assert!(res.is_err());
    }
}
