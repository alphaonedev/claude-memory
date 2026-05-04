// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_search` migration. See `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::cli::helpers::{human_age, id_short};
use crate::models::Tier;
use crate::{db, validate};
use anyhow::Result;
use clap::Args;
use std::path::Path;

/// Clap-derived arg shape for the `search` subcommand. Definition moved
/// from `main.rs` verbatim in W5b — fields and attrs unchanged.
#[derive(Args)]
pub struct SearchArgs {
    #[arg(allow_hyphen_values = true)]
    pub query: String,
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
    /// Filter by `metadata.agent_id` (exact match)
    #[arg(long)]
    pub agent_id: Option<String>,
    /// Task 1.5: querying agent's namespace position for scope-based
    /// visibility filtering.
    #[arg(long)]
    pub as_agent: Option<String>,
}

/// `search` handler. Mirrors `cmd_search` from `main.rs` verbatim except
/// every emit routes through `out.stdout` / `out.stderr` instead of
/// `println!` / `eprintln!`.
pub fn run(
    db_path: &Path,
    args: &SearchArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    // #197: validate agent_id filter values
    if let Some(ref aid) = args.agent_id {
        validate::validate_agent_id(aid)?;
    }
    // #151: validate --as-agent namespace
    if let Some(ref a) = args.as_agent {
        validate::validate_namespace(a)?;
    }
    let conn = db::open(db_path)?;
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let results = db::search(
        &conn,
        &args.query,
        args.namespace.as_deref(),
        tier.as_ref(),
        args.limit,
        None,
        args.since.as_deref(),
        args.until.as_deref(),
        args.tags.as_deref(),
        args.agent_id.as_deref(),
        args.as_agent.as_deref(),
    )?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(
                &serde_json::json!({"results": results, "count": results.len()})
            )?
        )?;
        return Ok(());
    }
    if results.is_empty() {
        writeln!(out.stderr, "no results for: {}", args.query)?;
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
    writeln!(out.stdout, "\n{} result(s)", results.len())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn default_args() -> SearchArgs {
        SearchArgs {
            query: "needle".to_string(),
            namespace: None,
            tier: None,
            limit: 20,
            since: None,
            until: None,
            tags: None,
            agent_id: None,
            as_agent: None,
        }
    }

    #[test]
    fn test_search_happy_path_text() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "haystack content");
        let args = default_args();
        {
            let mut out = env.output();
            run(&db, &args, false, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("needle title"), "got: {stdout}");
        assert!(stdout.contains("result(s)"));
    }

    #[test]
    fn test_search_happy_path_json() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "haystack content");
        let args = default_args();
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["count"].as_u64().unwrap() >= 1);
        assert!(v["results"].is_array());
    }

    #[test]
    fn test_search_no_results() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let args = default_args();
        {
            let mut out = env.output();
            run(&db, &args, false, &mut out).unwrap();
        }
        // Text branch: nothing on stdout, stderr carries the "no results".
        assert_eq!(env.stdout_str(), "");
        assert!(
            env.stderr_str().contains("no results for: needle"),
            "got: {}",
            env.stderr_str()
        );
    }

    #[test]
    fn test_search_with_namespace_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "ns-a", "needle in a", "content a");
        seed_memory(&db, "ns-b", "needle in b", "content b");
        let mut args = default_args();
        args.namespace = Some("ns-a".to_string());
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let results = v["results"].as_array().unwrap();
        for r in results {
            assert_eq!(r["namespace"].as_str().unwrap(), "ns-a");
        }
    }

    #[test]
    fn test_search_with_tier_filter() {
        // seed_memory uses tier=mid; the "long" filter excludes everything.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        args.tier = Some("long".to_string());
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_search_with_agent_id_filter() {
        // seed_memory writes agent_id="test-agent" into metadata; passing
        // a different agent_id excludes the row.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test", "needle title", "content");
        let mut args = default_args();
        args.agent_id = Some("other-agent".to_string());
        {
            let mut out = env.output();
            run(&db, &args, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["count"].as_u64().unwrap(), 0);

        // And the affirmative case: matching agent_id returns the row.
        let mut env2 = TestEnv::fresh();
        let db2 = env2.db_path.clone();
        seed_memory(&db2, "test", "needle title", "content");
        let mut args2 = default_args();
        args2.agent_id = Some("test-agent".to_string());
        {
            let mut out = env2.output();
            run(&db2, &args2, true, &mut out).unwrap();
        }
        let v2: serde_json::Value = serde_json::from_str(env2.stdout_str().trim()).unwrap();
        assert!(v2["count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn test_search_invalid_agent_id_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let mut args = default_args();
        // Empty agent_id is rejected by validate_agent_id.
        args.agent_id = Some(String::new());
        let mut out = env.output();
        let res = run(&db, &args, false, &mut out);
        assert!(res.is_err(), "expected validate_agent_id to reject empty");
    }

    #[test]
    fn test_search_invalid_as_agent_namespace_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let mut args = default_args();
        args.as_agent = Some(String::new());
        let mut out = env.output();
        let res = run(&db, &args, false, &mut out);
        assert!(res.is_err(), "expected validate_namespace to reject empty");
    }
}
