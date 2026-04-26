// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_consolidate` and `cmd_auto_consolidate` migrations. See
//! `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::cli::helpers::auto_namespace;
use crate::{db, identity, models, validate};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;

#[derive(Args)]
pub struct ConsolidateArgs {
    /// Comma-separated memory IDs
    pub ids: String,
    #[arg(long, short = 'T', allow_hyphen_values = true)]
    pub title: String,
    #[arg(long, short = 's', allow_hyphen_values = true)]
    pub summary: String,
    #[arg(long, short)]
    pub namespace: Option<String>,
}

#[derive(Args)]
pub struct AutoConsolidateArgs {
    /// Namespace to consolidate
    #[arg(long, short)]
    pub namespace: Option<String>,
    /// Only consolidate short-term memories
    #[arg(long, default_value_t = false)]
    pub short_only: bool,
    /// Minimum number of memories to trigger consolidation
    #[arg(long, default_value_t = 3)]
    pub min_count: usize,
    /// Dry run — show what would be consolidated without doing it
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

/// `consolidate` handler.
pub fn run(
    db_path: &Path,
    args: ConsolidateArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let ids: Vec<String> = args
        .ids
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let namespace = args.namespace.unwrap_or_else(auto_namespace);
    validate::validate_consolidate(&ids, &args.title, &args.summary, &namespace)?;
    let conn = db::open(db_path)?;
    let consolidator_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let new_id = db::consolidate(
        &conn,
        &ids,
        &args.title,
        &args.summary,
        &namespace,
        &Tier::Long,
        "cli",
        &consolidator_agent_id,
    )?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({"id": new_id, "consolidated": ids.len()})
        )?;
    } else {
        writeln!(
            out.stdout,
            "consolidated {} memories into: {}",
            ids.len(),
            new_id
        )?;
    }
    Ok(())
}

/// `auto-consolidate` handler.
#[allow(clippy::too_many_lines)]
pub fn run_auto(
    db_path: &Path,
    args: &AutoConsolidateArgs,
    json_out: bool,
    cli_agent_id: Option<&str>,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    let consolidator_agent_id = identity::resolve_agent_id(cli_agent_id, None)?;
    let tier_filter = if args.short_only {
        Some(Tier::Short)
    } else {
        None
    };
    let namespaces = if let Some(ref ns) = args.namespace {
        vec![models::NamespaceCount {
            namespace: ns.clone(),
            count: 0,
        }]
    } else {
        db::list_namespaces(&conn)?
    };

    let mut total = 0;
    let mut groups = Vec::new();

    for ns in &namespaces {
        let memories = db::list(
            &conn,
            Some(&ns.namespace),
            tier_filter.as_ref(),
            200,
            0,
            None,
            None,
            None,
            None,
            None,
        )?;
        if memories.len() < args.min_count {
            continue;
        }

        // Group by all tags (each memory appears in every tag group it belongs to)
        let mut tag_groups: std::collections::HashMap<String, Vec<&models::Memory>> =
            std::collections::HashMap::new();
        for mem in &memories {
            if mem.tags.is_empty() {
                tag_groups
                    .entry("_untagged".to_string())
                    .or_default()
                    .push(mem);
            } else {
                for tag in &mem.tags {
                    tag_groups.entry(tag.clone()).or_default().push(mem);
                }
            }
        }

        let mut consolidated_ids: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for (tag, group) in &tag_groups {
            // Skip memories already consolidated in another tag group
            let group: Vec<&&models::Memory> = group
                .iter()
                .filter(|m| !consolidated_ids.contains(&m.id))
                .collect();
            if group.len() < args.min_count {
                continue;
            }
            let ids: Vec<String> = group.iter().map(|m| m.id.clone()).collect();
            if args.dry_run {
                let titles: Vec<&str> = group.iter().map(|m| m.title.as_str()).collect();
                groups.push(serde_json::json!({"namespace": ns.namespace, "tag": tag, "count": group.len(), "titles": titles}));
            } else {
                let title = format!(
                    "Consolidated: {} ({} memories)",
                    if tag == "_untagged" {
                        &ns.namespace
                    } else {
                        tag
                    },
                    group.len()
                );
                let content: String = group
                    .iter()
                    .map(|m| format!("- {}: {}", m.title, &m.content[..m.content.len().min(200)]))
                    .collect::<Vec<_>>()
                    .join("\n");
                db::consolidate(
                    &conn,
                    &ids,
                    &title,
                    &content,
                    &ns.namespace,
                    &Tier::Long,
                    "auto-consolidate",
                    &consolidator_agent_id,
                )?;
                consolidated_ids.extend(ids);
                total += group.len();
            }
        }
    }

    if json_out {
        if args.dry_run {
            writeln!(
                out.stdout,
                "{}",
                serde_json::json!({"dry_run": true, "groups": groups})
            )?;
        } else {
            writeln!(out.stdout, "{}", serde_json::json!({"consolidated": total}))?;
        }
    } else if args.dry_run {
        writeln!(out.stdout, "dry run — would consolidate:")?;
        for g in &groups {
            writeln!(
                out.stdout,
                "  {} [{}]: {} memories",
                g["namespace"], g["tag"], g["count"]
            )?;
        }
    } else {
        writeln!(out.stdout, "auto-consolidated {total} memories")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn ns_args() -> ConsolidateArgs {
        ConsolidateArgs {
            ids: String::new(),
            title: "consolidated title".to_string(),
            summary: "merged summary".to_string(),
            namespace: Some("test-ns".to_string()),
        }
    }

    #[test]
    fn test_consolidate_happy_path() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "test-ns", "first", "alpha");
        let id2 = seed_memory(&db, "test-ns", "second", "beta");
        let mut args = ns_args();
        args.ids = format!("{id1},{id2}");
        {
            let mut out = env.output();
            run(&db, args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("consolidated 2 memories into:"));
    }

    #[test]
    fn test_consolidate_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "test-ns", "a1", "data1");
        let id2 = seed_memory(&db, "test-ns", "a2", "data2");
        let mut args = ns_args();
        args.ids = format!("{id1},{id2}");
        {
            let mut out = env.output();
            run(&db, args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["id"].is_string());
        assert_eq!(v["consolidated"].as_u64().unwrap(), 2);
    }

    #[test]
    fn test_consolidate_single_id_validation_error() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "test-ns", "lone", "only-one");
        let mut args = ns_args();
        args.ids = id1;
        let mut out = env.output();
        let res = run(&db, args, false, Some("test-agent"), &mut out);
        assert!(res.is_err(), "single id should fail validation");
    }

    #[test]
    fn test_consolidate_invalid_namespace() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id1 = seed_memory(&db, "test-ns", "x", "y");
        let id2 = seed_memory(&db, "test-ns", "x2", "y2");
        let mut args = ns_args();
        args.ids = format!("{id1},{id2}");
        // Reserved/empty namespace; validate_namespace rejects empty.
        args.namespace = Some(String::new());
        let mut out = env.output();
        let res = run(&db, args, false, Some("test-agent"), &mut out);
        assert!(res.is_err());
    }

    #[test]
    fn test_auto_consolidate_dry_run_lists_groups() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Seed several memories in the same ns so the threshold trips.
        for i in 0..4 {
            seed_memory(&db, "auto-ns", &format!("title-{i}"), &format!("body-{i}"));
        }
        let args = AutoConsolidateArgs {
            namespace: Some("auto-ns".to_string()),
            short_only: false,
            min_count: 3,
            dry_run: true,
        };
        {
            let mut out = env.output();
            run_auto(&db, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("dry run"));
    }

    #[test]
    fn test_auto_consolidate_below_min_count_no_op() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        // Only one memory — well below min_count=3.
        seed_memory(&db, "auto-ns", "lone", "only");
        let args = AutoConsolidateArgs {
            namespace: Some("auto-ns".to_string()),
            short_only: false,
            min_count: 3,
            dry_run: false,
        };
        {
            let mut out = env.output();
            run_auto(&db, &args, false, Some("test-agent"), &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("auto-consolidated 0"));
    }

    #[test]
    fn test_auto_consolidate_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        for i in 0..4 {
            seed_memory(&db, "auto-ns", &format!("t{i}"), &format!("b{i}"));
        }
        let args = AutoConsolidateArgs {
            namespace: Some("auto-ns".to_string()),
            short_only: false,
            min_count: 3,
            dry_run: false,
        };
        {
            let mut out = env.output();
            run_auto(&db, &args, true, Some("test-agent"), &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["consolidated"].as_u64().is_some());
    }
}
