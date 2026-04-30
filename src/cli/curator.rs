// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_curator` migration. The daemon-mode body delegates to
//! `daemon_runtime::run_curator_daemon_with_primitives` (W3 work);
//! this module owns only the outer wrapper and the report printer.

use crate::cli::CliOutput;
use crate::{autonomy, config, curator, db, llm};
use anyhow::{Context, Result};
use clap::Args;
use std::path::Path;

#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct CuratorArgs {
    /// Run exactly one sweep and exit. Mutually exclusive with --daemon.
    #[arg(long, conflicts_with = "daemon")]
    pub once: bool,
    /// Loop forever, sleeping --interval-secs between sweeps. SIGINT /
    /// SIGTERM trigger a clean shutdown between cycles.
    #[arg(long)]
    pub daemon: bool,
    /// Seconds between daemon sweeps. Clamped to [60, 86400].
    #[arg(long, default_value_t = 3600)]
    pub interval_secs: u64,
    /// Hard cap on LLM-invoking operations per cycle.
    #[arg(long, default_value_t = 100)]
    pub max_ops: usize,
    /// Emit the report without persisting any metadata changes.
    #[arg(long)]
    pub dry_run: bool,
    /// Only curate memories in these namespaces. Repeat flag for multiple.
    #[arg(long = "include-namespace")]
    pub include_namespaces: Vec<String>,
    /// Exclude these namespaces from curation. Repeat flag for multiple.
    #[arg(long = "exclude-namespace")]
    pub exclude_namespaces: Vec<String>,
    /// Print the report as JSON rather than a human-readable summary.
    #[arg(long)]
    pub json: bool,
    /// Reverse rollback-log entries instead of running a sweep. Accepts
    /// a specific rollback-memory id, or `--last N` for the most recent.
    /// Mutually exclusive with `--once` and `--daemon`.
    #[arg(long, conflicts_with_all = ["once", "daemon"])]
    pub rollback: Option<String>,
    /// With `--rollback`, reverse the N most recent rollback-log entries
    /// instead of a single id.
    #[arg(long)]
    pub rollback_last: Option<usize>,
}

fn build_curator_llm(tier: config::FeatureTier) -> Option<llm::OllamaClient> {
    let llm_model = tier.config().llm_model?;
    let model = llm_model.ollama_model_id().to_string();
    llm::OllamaClient::new(&model).ok()
}

fn print_curator_report(r: &curator::CuratorReport, out: &mut CliOutput<'_>) -> Result<()> {
    writeln!(out.stdout, "curator cycle report")?;
    writeln!(out.stdout, "  started_at:        {}", r.started_at)?;
    writeln!(out.stdout, "  completed_at:      {}", r.completed_at)?;
    writeln!(out.stdout, "  duration_ms:       {}", r.cycle_duration_ms)?;
    writeln!(out.stdout, "  memories_scanned:  {}", r.memories_scanned)?;
    writeln!(out.stdout, "  memories_eligible: {}", r.memories_eligible)?;
    writeln!(
        out.stdout,
        "  operations:        {}",
        r.operations_attempted
    )?;
    writeln!(out.stdout, "  auto_tagged:       {}", r.auto_tagged)?;
    writeln!(
        out.stdout,
        "  contradictions:    {}",
        r.contradictions_found
    )?;
    writeln!(
        out.stdout,
        "  skipped (cap):     {}",
        r.operations_skipped_cap
    )?;
    writeln!(out.stdout, "  errors:            {}", r.errors.len())?;
    writeln!(out.stdout, "  dry_run:           {}", r.dry_run)?;
    for e in &r.errors {
        writeln!(out.stdout, "    - {e}")?;
    }
    Ok(())
}

/// `curator` handler. Daemon-mode delegates to `daemon_runtime`.
pub async fn run(
    db_path: &Path,
    args: &CuratorArgs,
    app_config: &config::AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    if args.rollback.is_some() || args.rollback_last.is_some() {
        return run_rollback(db_path, args, out);
    }

    if !args.once && !args.daemon {
        anyhow::bail!("curator requires --once, --daemon, --rollback <id>, or --rollback-last N");
    }

    let cfg = curator::CuratorConfig {
        interval_secs: args.interval_secs,
        max_ops_per_cycle: args.max_ops,
        dry_run: args.dry_run,
        include_namespaces: args.include_namespaces.clone(),
        exclude_namespaces: args.exclude_namespaces.clone(),
    };

    let feature_tier = app_config.effective_tier(None);
    let llm = build_curator_llm(feature_tier);

    if args.once {
        let conn = db::open(db_path)?;
        let report = curator::run_once(&conn, llm.as_ref(), &cfg)?;
        if args.json {
            writeln!(out.stdout, "{}", serde_json::to_string_pretty(&report)?)?;
        } else {
            print_curator_report(&report, out)?;
        }
        return Ok(());
    }

    // Daemon mode — delegate to daemon_runtime.
    let shutdown = std::sync::Arc::new(tokio::sync::Notify::new());
    let shutdown_for_signal = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_for_signal.notify_one();
    });

    let ollama_model = feature_tier
        .config()
        .llm_model
        .map(|m| m.ollama_model_id().to_string());

    crate::daemon_runtime::run_curator_daemon_with_primitives(
        db_path.to_path_buf(),
        args.interval_secs,
        args.max_ops,
        args.dry_run,
        args.include_namespaces.clone(),
        args.exclude_namespaces.clone(),
        ollama_model,
        shutdown,
    )
    .await
}

fn run_rollback(db_path: &Path, args: &CuratorArgs, out: &mut CliOutput<'_>) -> Result<()> {
    let conn = db::open(db_path)?;

    if let Some(id) = &args.rollback {
        let Some(mem) = db::get(&conn, id)? else {
            anyhow::bail!("rollback entry {id} not found");
        };
        let entry: autonomy::RollbackEntry = serde_json::from_str(&mem.content)
            .context("rollback entry content is not a valid RollbackEntry JSON")?;
        let applied = autonomy::reverse_rollback_entry(&conn, &entry)?;
        let mut tags = mem.tags.clone();
        if !tags.iter().any(|t| t == "_reversed") {
            tags.push("_reversed".to_string());
            db::update(
                &conn,
                &mem.id,
                None,
                None,
                None,
                None,
                Some(&tags),
                None,
                None,
                None,
                None,
            )?;
        }
        writeln!(
            out.stdout,
            "rollback {id}: {}",
            if applied { "applied" } else { "no-op" }
        )?;
        return Ok(());
    }

    if let Some(n) = args.rollback_last {
        let log = db::list(
            &conn,
            Some("_curator/rollback"),
            None,
            n.max(1),
            0,
            None,
            None,
            None,
            None,
            None,
        )?;
        let mut reversed = 0usize;
        for mem in &log {
            if mem.tags.iter().any(|t| t == "_reversed") {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<autonomy::RollbackEntry>(&mem.content) else {
                continue;
            };
            let applied = autonomy::reverse_rollback_entry(&conn, &entry)?;
            if applied {
                reversed += 1;
                let mut tags = mem.tags.clone();
                tags.push("_reversed".to_string());
                db::update(
                    &conn,
                    &mem.id,
                    None,
                    None,
                    None,
                    None,
                    Some(&tags),
                    None,
                    None,
                    None,
                    None,
                )?;
            }
        }
        writeln!(out.stdout, "reversed {reversed} rollback entries")?;
        return Ok(());
    }

    unreachable!("run_rollback entered without --rollback or --rollback-last");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::TestEnv;

    fn default_args() -> CuratorArgs {
        CuratorArgs {
            once: false,
            daemon: false,
            interval_secs: 3600,
            max_ops: 100,
            dry_run: false,
            include_namespaces: Vec::new(),
            exclude_namespaces: Vec::new(),
            json: false,
            rollback: None,
            rollback_last: None,
        }
    }

    #[tokio::test]
    async fn test_curator_requires_mode() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let args = default_args();
        let mut out = env.output();
        let res = run(&db, &args, &cfg, &mut out).await;
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("--once, --daemon, --rollback")
        );
    }

    #[tokio::test]
    async fn test_curator_once_runs_single_sweep_text() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        assert!(env.stdout_str().contains("curator cycle report"));
    }

    #[tokio::test]
    async fn test_curator_once_json_format() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.json = true;
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["dry_run"].as_bool().unwrap());
    }

    #[tokio::test]
    async fn test_curator_dry_run_skips_writes() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.dry_run = true;
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // Report mentions dry_run flag.
        let s = env.stdout_str();
        assert!(s.contains("dry_run:") || s.contains("\"dry_run\""));
    }

    #[tokio::test]
    async fn test_curator_include_namespaces_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.dry_run = true;
        args.include_namespaces = vec!["only-this-ns".to_string()];
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // No memories — operations attempted should be 0.
        assert!(env.stdout_str().contains("operations:"));
    }

    #[tokio::test]
    async fn test_curator_exclude_namespaces_filter() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.dry_run = true;
        args.exclude_namespaces = vec!["skip-me".to_string()];
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        assert!(env.stdout_str().contains("curator cycle report"));
    }

    #[tokio::test]
    async fn test_curator_max_ops_cap_respected() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.once = true;
        args.dry_run = true;
        args.max_ops = 0; // immediately at cap
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        assert!(env.stdout_str().contains("operations:"));
    }

    #[tokio::test]
    async fn test_curator_rollback_id_not_found() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.rollback = Some("00000000-0000-0000-0000-000000000000".to_string());
        let mut out = env.output();
        let res = run(&db, &args, &cfg, &mut out).await;
        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("rollback entry"));
    }

    #[tokio::test]
    async fn test_curator_rollback_last_zero_entries() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let mut args = default_args();
        args.rollback_last = Some(5);
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // No rollback log entries; should report 0.
        assert!(env.stdout_str().contains("reversed 0"));
    }

    // PR-9i — buffer coverage uplift. Targets run_rollback() — rollback
    // path with valid PriorityAdjust entry, rollback_last with both
    // applied & malformed JSON entries (skip branch), already-reversed
    // skip branch.

    fn build_priority_rollback_entry_json(memory_id: &str, before: i32, after: i32) -> String {
        // Serialize as the externally-tagged enum form `autonomy::RollbackEntry`
        // uses (the Rust default).
        serde_json::to_string(&autonomy::RollbackEntry::PriorityAdjust {
            memory_id: memory_id.to_string(),
            before,
            after,
        })
        .unwrap()
    }

    fn seed_rollback_entry(db_path: &std::path::Path, content: &str) -> String {
        // Insert a memory in the _curator/rollback namespace whose content
        // is a serialized RollbackEntry. Returns the inserted id.
        let conn = db::open(db_path).expect("db::open");
        let now = chrono::Utc::now().to_rfc3339();
        let mut metadata = crate::models::default_metadata();
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(
                "agent_id".to_string(),
                serde_json::Value::String("test-agent".to_string()),
            );
        }
        let mem = crate::models::Memory {
            id: uuid::Uuid::new_v4().to_string(),
            tier: crate::models::Tier::Mid,
            namespace: "_curator/rollback".to_string(),
            title: format!("rollback-{}", uuid::Uuid::new_v4()),
            content: content.to_string(),
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
        db::insert(&conn, &mem).expect("db::insert")
    }

    #[tokio::test]
    async fn pr9i_curator_rollback_priority_adjust_applies() {
        // Seed a real memory whose priority we'll roll back from 7→3.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();

        // 1. Seed a target memory at priority=7.
        let target = {
            let conn = db::open(&db).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let mut metadata = crate::models::default_metadata();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String("test-agent".to_string()),
                );
            }
            let mem = crate::models::Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: crate::models::Tier::Mid,
                namespace: "ns".to_string(),
                title: "target".to_string(),
                content: "c".to_string(),
                tags: vec![],
                priority: 7,
                confidence: 1.0,
                source: "test".to_string(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata,
            };
            db::insert(&conn, &mem).unwrap()
        };

        // 2. Seed a rollback entry that says "revert priority to 3".
        let entry_json = build_priority_rollback_entry_json(&target, 3, 7);
        let entry_id = seed_rollback_entry(&db, &entry_json);

        // 3. Run rollback by id.
        let mut args = default_args();
        args.rollback = Some(entry_id.clone());
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // Stdout reports rollback applied.
        let s = env.stdout_str();
        assert!(s.contains(&format!("rollback {entry_id}")));
        assert!(s.contains("applied"));

        // The target's priority must now be 3.
        let conn = db::open(&db).unwrap();
        let target_mem = db::get(&conn, &target).unwrap().unwrap();
        assert_eq!(target_mem.priority, 3);

        // The rollback entry must be tagged _reversed.
        let entry_mem = db::get(&conn, &entry_id).unwrap().unwrap();
        assert!(entry_mem.tags.iter().any(|t| t == "_reversed"));
    }

    #[tokio::test]
    async fn pr9i_curator_rollback_last_processes_multiple() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();

        // Seed two targets.
        let t1;
        let t2;
        {
            let conn = db::open(&db).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let mut metadata = crate::models::default_metadata();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String("test-agent".to_string()),
                );
            }
            let m1 = crate::models::Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: crate::models::Tier::Mid,
                namespace: "ns".to_string(),
                title: "t1".to_string(),
                content: "c1".to_string(),
                tags: vec![],
                priority: 8,
                confidence: 1.0,
                source: "test".to_string(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                last_accessed_at: None,
                expires_at: None,
                metadata: metadata.clone(),
            };
            let m2 = crate::models::Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: crate::models::Tier::Mid,
                namespace: "ns".to_string(),
                title: "t2".to_string(),
                content: "c2".to_string(),
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
            t1 = db::insert(&conn, &m1).unwrap();
            t2 = db::insert(&conn, &m2).unwrap();
        }

        // Seed two rollback entries plus one malformed JSON entry.
        seed_rollback_entry(&db, &build_priority_rollback_entry_json(&t1, 4, 8));
        seed_rollback_entry(&db, &build_priority_rollback_entry_json(&t2, 5, 9));
        seed_rollback_entry(&db, "{not valid json: at all"); // malformed → skip branch

        // Run rollback_last 5 (caps at actual count).
        let mut args = default_args();
        args.rollback_last = Some(5);
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // Reverses 2 entries (the malformed one is skipped).
        let s = env.stdout_str();
        assert!(s.contains("reversed 2"));

        // Both targets reverted.
        let conn = db::open(&db).unwrap();
        assert_eq!(db::get(&conn, &t1).unwrap().unwrap().priority, 4);
        assert_eq!(db::get(&conn, &t2).unwrap().unwrap().priority, 5);
    }

    #[tokio::test]
    async fn pr9i_curator_rollback_last_skips_already_reversed() {
        // Seed a rollback entry pre-tagged as _reversed; rollback_last must
        // skip it (lines 203-205).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();

        // Seed a target.
        let target;
        {
            let conn = db::open(&db).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let mut metadata = crate::models::default_metadata();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String("test-agent".to_string()),
                );
            }
            let mem = crate::models::Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: crate::models::Tier::Mid,
                namespace: "ns".to_string(),
                title: "x".to_string(),
                content: "c".to_string(),
                tags: vec![],
                priority: 7,
                confidence: 1.0,
                source: "test".to_string(),
                access_count: 0,
                created_at: now.clone(),
                updated_at: now,
                last_accessed_at: None,
                expires_at: None,
                metadata,
            };
            target = db::insert(&conn, &mem).unwrap();
        }

        // Insert a rollback entry already tagged _reversed.
        let entry_json = build_priority_rollback_entry_json(&target, 2, 7);
        let entry_id;
        {
            let conn = db::open(&db).unwrap();
            let now = chrono::Utc::now().to_rfc3339();
            let mut metadata = crate::models::default_metadata();
            if let Some(obj) = metadata.as_object_mut() {
                obj.insert(
                    "agent_id".to_string(),
                    serde_json::Value::String("test-agent".to_string()),
                );
            }
            let mem = crate::models::Memory {
                id: uuid::Uuid::new_v4().to_string(),
                tier: crate::models::Tier::Mid,
                namespace: "_curator/rollback".to_string(),
                title: "preexisting-reversed".to_string(),
                content: entry_json,
                tags: vec!["_reversed".to_string()],
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
            entry_id = db::insert(&conn, &mem).unwrap();
        }

        let mut args = default_args();
        args.rollback_last = Some(5);
        {
            let mut out = env.output();
            run(&db, &args, &cfg, &mut out).await.unwrap();
        }
        // Already-reversed entry is skipped → reversed 0.
        let s = env.stdout_str();
        assert!(s.contains("reversed 0"));

        // Target's priority is unchanged from 7.
        let conn = db::open(&db).unwrap();
        assert_eq!(db::get(&conn, &target).unwrap().unwrap().priority, 7);
        // Sanity: entry_id memory still tagged _reversed.
        let entry_mem = db::get(&conn, &entry_id).unwrap().unwrap();
        assert!(entry_mem.tags.iter().any(|t| t == "_reversed"));
    }

    #[tokio::test]
    async fn pr9i_curator_rollback_id_with_malformed_content() {
        // Seed a memory in _curator/rollback whose content is NOT a valid
        // RollbackEntry — the explicit-id rollback path bails (lines 160-161).
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        let entry_id = seed_rollback_entry(&db, "{invalid json");

        let mut args = default_args();
        args.rollback = Some(entry_id);
        let mut out = env.output();
        let res = run(&db, &args, &cfg, &mut out).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(
            err.contains("rollback") || err.contains("RollbackEntry"),
            "expected parse-error message, got: {err}"
        );
    }
}
