// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_gc`, `cmd_stats`, and `cmd_namespaces` migrations. See
//! `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::{config, db};
use anyhow::Result;
use std::path::Path;

/// `gc` handler.
pub fn run_gc(
    db_path: &Path,
    json_out: bool,
    app_config: &config::AppConfig,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let conn = db::open(db_path)?;
    let count = db::gc(&conn, app_config.effective_archive_on_gc())?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::json!({"expired_deleted": count})
        )?;
    } else {
        writeln!(out.stdout, "expired memories deleted: {count}")?;
    }
    Ok(())
}

/// `stats` handler.
pub fn run_stats(db_path: &Path, json_out: bool, out: &mut CliOutput<'_>) -> Result<()> {
    let conn = db::open(db_path)?;
    let stats = db::stats(&conn, db_path)?;
    if json_out {
        writeln!(out.stdout, "{}", serde_json::to_string(&stats)?)?;
        return Ok(());
    }
    writeln!(out.stdout, "total memories: {}", stats.total)?;
    writeln!(out.stdout, "expiring within 1h: {}", stats.expiring_soon)?;
    writeln!(out.stdout, "links: {}", stats.links_count)?;
    writeln!(out.stdout, "database size: {} bytes", stats.db_size_bytes)?;
    writeln!(out.stdout, "\nby tier:")?;
    for t in &stats.by_tier {
        writeln!(out.stdout, "  {}: {}", t.tier, t.count)?;
    }
    writeln!(out.stdout, "\nby namespace:")?;
    for ns in &stats.by_namespace {
        writeln!(out.stdout, "  {}: {}", ns.namespace, ns.count)?;
    }
    Ok(())
}

/// `namespaces` handler.
pub fn run_namespaces(db_path: &Path, json_out: bool, out: &mut CliOutput<'_>) -> Result<()> {
    let conn = db::open(db_path)?;
    let ns = db::list_namespaces(&conn)?;
    if json_out {
        writeln!(
            out.stdout,
            "{}",
            serde_json::to_string(&serde_json::json!({"namespaces": ns}))?
        )?;
        return Ok(());
    }
    if ns.is_empty() {
        writeln!(out.stderr, "no namespaces")?;
    } else {
        for n in &ns {
            writeln!(out.stdout, "  {}: {} memories", n.namespace, n.count)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    #[test]
    fn test_gc_empty_db() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        {
            let mut out = env.output();
            run_gc(&db, false, &cfg, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("expired memories deleted: 0"));
    }

    #[test]
    fn test_gc_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let cfg = config::AppConfig::default();
        {
            let mut out = env.output();
            run_gc(&db, true, &cfg, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["expired_deleted"].as_u64().unwrap(), 0);
    }

    #[test]
    fn test_gc_with_data_present() {
        // Seed a memory with normal future expiry. gc should be a no-op.
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "test-ns", "live", "still kicking");
        let cfg = config::AppConfig::default();
        {
            let mut out = env.output();
            run_gc(&db, false, &cfg, &mut out).unwrap();
        }
        assert!(env.stdout_str().contains("expired memories deleted:"));
    }

    #[test]
    fn test_stats_on_empty_db() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        {
            let mut out = env.output();
            run_stats(&db, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("total memories: 0"));
        assert!(s.contains("links: 0"));
    }

    #[test]
    fn test_stats_with_data() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "ns-a", "t1", "c1");
        seed_memory(&db, "ns-b", "t2", "c2");
        {
            let mut out = env.output();
            run_stats(&db, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("total memories: 2"));
        assert!(s.contains("by tier:"));
        assert!(s.contains("by namespace:"));
    }

    #[test]
    fn test_stats_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "ns", "t", "c");
        {
            let mut out = env.output();
            run_stats(&db, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["total"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_stats_by_tier_breakdown() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "ns", "t", "c");
        {
            let mut out = env.output();
            run_stats(&db, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert!(v["by_tier"].is_array());
        assert!(v["by_namespace"].is_array());
    }

    #[test]
    fn test_namespaces_empty_writes_stderr() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        {
            let mut out = env.output();
            run_namespaces(&db, false, &mut out).unwrap();
        }
        assert!(env.stderr_str().contains("no namespaces"));
        assert_eq!(env.stdout_str(), "");
    }

    #[test]
    fn test_namespaces_with_data() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "alpha", "t", "c");
        seed_memory(&db, "beta", "t2", "c2");
        {
            let mut out = env.output();
            run_namespaces(&db, false, &mut out).unwrap();
        }
        let s = env.stdout_str();
        assert!(s.contains("alpha"));
        assert!(s.contains("beta"));
    }

    #[test]
    fn test_namespaces_json_output() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        seed_memory(&db, "alpha", "t", "c");
        {
            let mut out = env.output();
            run_namespaces(&db, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        let arr = v["namespaces"].as_array().unwrap();
        assert!(!arr.is_empty());
    }

    #[test]
    fn test_namespaces_json_empty_array() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        {
            let mut out = env.output();
            run_namespaces(&db, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["namespaces"].as_array().unwrap().len(), 0);
    }
}
