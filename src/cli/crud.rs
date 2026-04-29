// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_get`, `cmd_list`, `cmd_delete` migrations. See `cli::store` for
//! the design pattern.
//!
//! ## Public surface
//!
//! ```ignore
//! pub fn cmd_get(db_path: &Path, args: &GetArgs, json_out: bool, out: &mut CliOutput<'_>) -> Result<()>;
//! pub fn cmd_list(db_path: &Path, args: &ListArgs, json_out: bool, app_config: &config::AppConfig, out: &mut CliOutput<'_>) -> Result<()>;
//! pub fn cmd_delete(db_path: &Path, args: &DeleteArgs, json_out: bool, cli_agent_id: Option<&str>, out: &mut CliOutput<'_>) -> Result<()>;
//! ```

use crate::cli::CliOutput;
use crate::cli::governance::{GovernanceOutcome, enforce as enforce_governance};
use crate::cli::helpers::{human_age, id_short};
use crate::{config, db, identity, models, validate};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;

#[derive(Args)]
pub struct GetArgs {
    pub id: String,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long, short)]
    pub namespace: Option<String>,
    #[arg(long, short)]
    pub tier: Option<String>,
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub offset: usize,
    /// Filter by `metadata.agent_id` (exact match)
    #[arg(long)]
    pub agent_id: Option<String>,
}

#[derive(Args)]
pub struct DeleteArgs {
    pub id: String,
}

/// `get` handler. Looks up by full id then prefix; prints memory + links.
pub fn cmd_get(
    db_path: &Path,
    args: &GetArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    if let Some(mem) = db::resolve_id(&conn, &args.id)? {
        let links = db::get_links(&conn, &mem.id).unwrap_or_default();
        if json_out {
            writeln!(
                out.stdout,
                "{}",
                serde_json::to_string(&serde_json::json!({"memory": mem, "links": links}))?
            )?;
        } else {
            writeln!(out.stdout, "{}", serde_json::to_string_pretty(&mem)?)?;
            if !links.is_empty() {
                writeln!(out.stdout, "\nlinks:")?;
                for l in &links {
                    writeln!(
                        out.stdout,
                        "  {} --[{}]--> {}",
                        l.source_id, l.relation, l.target_id
                    )?;
                }
            }
        }
    } else {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    }
    Ok(())
}

/// `list` handler.
#[allow(clippy::too_many_lines)]
pub fn cmd_list(
    db_path: &Path,
    args: &ListArgs,
    json_out: bool,
    app_config: &config::AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    if let Some(ref aid) = args.agent_id {
        validate::validate_agent_id(aid)?;
    }
    let conn = db::open(db_path)?;
    let _ = db::gc_if_needed(&conn, app_config.effective_archive_on_gc());
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::list(
        &conn,
        args.namespace.as_deref(),
        tier.as_ref(),
        args.limit,
        args.offset,
        None,
        args.since.as_deref(),
        args.until.as_deref(),
        args.tags.as_deref(),
        args.agent_id.as_deref(),
    )?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(
                &serde_json::json!({"memories": results, "count": results.len()})
            )?
        )?;
        return Ok(());
    }
    if results.is_empty() {
        writeln!(out.stderr, "no memories stored")?;
        return Ok(());
    }
    for mem in &results {
        let age = human_age(&mem.updated_at);
        writeln!(
            out.stdout,
            "[{}/{}] {} (p={}, ns={}, {})",
            mem.tier,
            id_short(&mem.id),
            mem.title,
            mem.priority,
            mem.namespace,
            age
        )?;
    }
    writeln!(out.stdout, "\n{} memory(ies)", results.len())?;
    Ok(())
}

/// `delete` handler.
pub fn cmd_delete(
    db_path: &Path,
    args: &DeleteArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    validate::validate_id(&args.id)?;
    let conn = db::open(db_path)?;
    let target = db::resolve_id(&conn, &args.id)?;
    let Some(target) = target else {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    };

    {
        use models::GovernedAction;
        let caller_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
        let mem_owner = target
            .metadata
            .get("agent_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let payload = serde_json::json!({"id": target.id, "title": target.title});
        match enforce_governance(
            &conn,
            GovernedAction::Delete,
            &target.namespace,
            &caller_agent_id,
            Some(&target.id),
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

    if db::delete(&conn, &target.id)? {
        if json_out {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({"deleted": true, "id": target.id})
            )?;
        } else {
            writeln!(out.stdout, "deleted: {}", target.id)?;
        }
    } else {
        writeln!(out.stderr, "not found: {}", args.id)?;
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn list_args() -> ListArgs {
        ListArgs {
            namespace: None,
            tier: None,
            limit: 20,
            since: None,
            until: None,
            tags: None,
            offset: 0,
            agent_id: None,
        }
    }

    // ---------------- get ---------------------------------------------

    #[test]
    fn test_get_by_full_id() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "title", "content");
        {
            let mut out = env.output();
            cmd_get(&db, &GetArgs { id: id.clone() }, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["memory"]["id"].as_str().unwrap(), id);
        assert_eq!(v["memory"]["title"].as_str().unwrap(), "title");
    }

    #[test]
    fn test_get_by_prefix() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "title", "content");
        let prefix = id[..8].to_string();
        {
            let mut out = env.output();
            cmd_get(&db, &GetArgs { id: prefix }, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["memory"]["id"].as_str().unwrap(), id);
    }

    // process::exit kills the test runner. Use a child-style sentinel
    // by validating the id-format error path, which `cmd_get` raises
    // before the not-found exit branch.
    #[test]
    fn test_get_invalid_id_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Malformed id with embedded null byte fails validate_id before
        // the lookup, so we never hit process::exit.
        let bad = "bad\0id".to_string();
        let mut out = env.output();
        let res = cmd_get(&db, &GetArgs { id: bad }, false, &mut out);
        assert!(res.is_err());
    }

    // Non-existent id triggers process::exit; covered by integration
    // suite that spawns the binary. In-process we can only assert the
    // helper returned with the not-found stderr message before exiting,
    // which is unreachable here.

    #[test]
    fn test_get_includes_links() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "ns", "a", "ca");
        let id2 = seed_memory(&db, "ns", "b", "cb");
        {
            let conn = db::open(&db).unwrap();
            db::create_link(&conn, &id1, &id2, "supersedes").unwrap();
        }
        {
            let mut out = env.output();
            cmd_get(&db, &GetArgs { id: id1.clone() }, false, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        // Pretty text branch prints "links:" + each pair.
        assert!(stdout.contains("links:"), "got: {stdout}");
        assert!(stdout.contains("supersedes"), "got: {stdout}");
    }

    #[test]
    fn test_get_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns-j", "tt", "cc");
        {
            let mut out = env.output();
            cmd_get(&db, &GetArgs { id }, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["memory"].is_object());
        assert!(v["links"].is_array());
    }

    #[test]
    fn test_get_text_output_when_no_links() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns-t", "tt", "cc");
        {
            let mut out = env.output();
            cmd_get(&db, &GetArgs { id }, false, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        // Pretty-printed body has 2-space indents.
        assert!(stdout.contains("\"title\": \"tt\""), "got: {stdout}");
        // No links section when there are no links.
        assert!(!stdout.contains("links:"));
    }

    // ---------------- list --------------------------------------------

    #[test]
    fn test_list_empty_db() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Materialize schema with a row, then forget it so the db has 0 rows.
        let _ = seed_memory(&db, "ns", "t", "c");
        {
            let conn = db::open(&db).unwrap();
            db::forget(&conn, Some("ns"), None, None, false).unwrap();
        }
        let cfg = config::AppConfig::default();
        let args = list_args();
        {
            let mut out = env.output();
            cmd_list(&db, &args, false, &cfg, &mut out).unwrap();
        }
        // text branch writes the empty-state message to stderr.
        assert!(
            env.stderr_str().contains("no memories stored"),
            "got: {}",
            env.stderr_str()
        );
    }

    #[test]
    fn test_list_with_namespace_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "alpha", "a", "ca");
        let _ = seed_memory(&db, "beta", "b", "cb");
        let cfg = config::AppConfig::default();
        let mut args = list_args();
        args.namespace = Some("alpha".to_string());
        {
            let mut out = env.output();
            cmd_list(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0]["namespace"].as_str().unwrap(), "alpha");
    }

    #[test]
    fn test_list_with_tier_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "a", "ca");
        // Promote one to long via direct update so we have a tier mix.
        let id_long = seed_memory(&db, "ns", "b-long", "cb");
        {
            let conn = db::open(&db).unwrap();
            db::update(
                &conn,
                &id_long,
                None,
                None,
                Some(&Tier::Long),
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        }
        let cfg = config::AppConfig::default();
        let mut args = list_args();
        args.tier = Some("long".to_string());
        {
            let mut out = env.output();
            cmd_list(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 1);
        assert_eq!(mems[0]["tier"].as_str().unwrap(), "long");
    }

    #[test]
    fn test_list_with_pagination_offset_limit() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        for i in 0..5 {
            let _ = seed_memory(&db, "ns", &format!("t-{i}"), "c");
        }
        let cfg = config::AppConfig::default();
        let mut args = list_args();
        args.limit = 2;
        args.offset = 1;
        {
            let mut out = env.output();
            cmd_list(&db, &args, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let mems = v["memories"].as_array().unwrap();
        assert_eq!(mems.len(), 2);
    }

    #[test]
    fn test_list_invalid_agent_id_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = list_args();
        args.agent_id = Some("has spaces".to_string());
        let mut out = env.output();
        let res = cmd_list(&db, &args, false, &cfg, &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_list_text_output_includes_short_id_and_age() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns-t", "the-title", "c");
        let cfg = config::AppConfig::default();
        let args = list_args();
        {
            let mut out = env.output();
            cmd_list(&db, &args, false, &cfg, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("the-title"), "got: {stdout}");
        assert!(stdout.contains("ns=ns-t"), "got: {stdout}");
        assert!(stdout.contains("memory(ies)"), "got: {stdout}");
    }

    // ---------------- delete ------------------------------------------

    #[test]
    fn test_delete_happy_path() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        {
            let mut out = env.output();
            cmd_delete(
                &db,
                &DeleteArgs { id: id.clone() },
                false,
                Some("test-agent"),
                &mut out,
            )
            .unwrap();
        }
        assert!(
            env.stdout_str().contains("deleted"),
            "got: {}",
            env.stdout_str()
        );
        let conn = db::open(&db).unwrap();
        assert!(db::get(&conn, &id).unwrap().is_none());
    }

    #[test]
    fn test_delete_by_prefix() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id = seed_memory(&db, "ns", "tt", "cc");
        let prefix = id[..8].to_string();
        {
            let mut out = env.output();
            cmd_delete(
                &db,
                &DeleteArgs { id: prefix },
                true,
                Some("test-agent"),
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_bool().unwrap(), true);
        assert_eq!(v["id"].as_str().unwrap(), id);
    }

    #[test]
    fn test_delete_governance_pending_returns_pending_status() {
        use crate::models::{ApproverType, GovernanceLevel, GovernancePolicy};
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Seed a memory in 'gov-ns' first so resolve_id finds something.
        let id = seed_memory(&db, "gov-ns", "tt", "cc");
        // Now seed a governance policy that gates delete behind Approve.
        let policy = GovernancePolicy {
            write: GovernanceLevel::Any,
            promote: GovernanceLevel::Any,
            delete: GovernanceLevel::Approve,
            approver: ApproverType::Human,
        };
        let conn = db::open(&db).unwrap();
        let now = chrono::Utc::now().to_rfc3339();
        let mut metadata = models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String("alice".to_string()),
            );
            obj.insert(
                "governance".to_string(),
                serde_json::to_value(&policy).unwrap(),
            );
        }
        let standard = models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: Tier::Long,
            namespace: "_standards-gov-ns".to_string(),
            title: "standard for gov-ns".to_string(),
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
        db::set_namespace_standard(&conn, "gov-ns", &standard_id, None).unwrap();
        drop(conn);

        {
            let mut out = env.output();
            cmd_delete(
                &db,
                &DeleteArgs { id: id.clone() },
                true,
                Some("bob"),
                &mut out,
            )
            .unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["status"].as_str().unwrap(), "pending");
        assert_eq!(v["action"].as_str().unwrap(), "delete");
        // Memory must NOT be deleted on Pending.
        let conn = db::open(&db).unwrap();
        assert!(db::get(&conn, &id).unwrap().is_some());
    }
}
