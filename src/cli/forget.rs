// Copyright 2026 AlphaOne LLC
// SPDX-License-Identifier: Apache-2.0

//! `cmd_forget` migration. See `cli::store` for the design pattern.

use crate::cli::CliOutput;
use crate::{db, models};
use anyhow::Result;
use clap::Args;
use models::Tier;
use std::path::Path;

#[derive(Args)]
pub struct ForgetArgs {
    #[arg(long, short)]
    pub namespace: Option<String>,
    #[arg(long, short)]
    pub pattern: Option<String>,
    #[arg(long, short)]
    pub tier: Option<String>,
}

/// `forget` handler. Deletes (and archives) memories matching at least
/// one of namespace/pattern/tier. CLI always passes `archive=true`.
pub fn cmd_forget(
    db_path: &Path,
    args: &ForgetArgs,
    json_out: bool,
    out: &mut CliOutput<'_>,
) -> Result<()> {
    let tier = args.tier.as_deref().and_then(Tier::from_str);
    let conn = db::open(db_path)?;
    match db::forget(
        &conn,
        args.namespace.as_deref(),
        args.pattern.as_deref(),
        tier.as_ref(),
        true, // always archive from CLI
    ) {
        Ok(n) => {
            if json_out {
                writeln!(out.stdout, "{}", serde_json::json!({"deleted": n}))?;
            } else {
                writeln!(out.stdout, "forgot {n} memories")?;
            }
        }
        Err(e) => {
            writeln!(out.stderr, "error: {e}")?;
            std::process::exit(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::test_utils::{TestEnv, seed_memory};

    fn args() -> ForgetArgs {
        ForgetArgs {
            namespace: None,
            pattern: None,
            tier: None,
        }
    }

    #[test]
    fn test_forget_by_namespace() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "alpha", "a", "ca");
        let _ = seed_memory(&db, "beta", "b", "cb");
        let mut a = args();
        a.namespace = Some("alpha".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
        // beta still present.
        let conn = db::open(&db).unwrap();
        let still = db::list(
            &conn,
            Some("beta"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(still.len(), 1);
    }

    #[test]
    fn test_forget_by_pattern() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "apple pie", "yum");
        let _ = seed_memory(&db, "ns", "banana split", "also yum");
        let mut a = args();
        a.pattern = Some("apple".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_forget_by_tier() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let id_long = seed_memory(&db, "ns", "long-row", "x");
        let _ = seed_memory(&db, "ns", "mid-row", "y");
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
        let mut a = args();
        a.tier = Some("long".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
    }

    #[test]
    fn test_forget_combined_filters() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "alpha", "apple-1", "x");
        let _ = seed_memory(&db, "beta", "apple-2", "y");
        let _ = seed_memory(&db, "alpha", "banana", "z");
        let mut a = args();
        a.namespace = Some("alpha".to_string());
        a.pattern = Some("apple".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, true, &mut out).unwrap();
        }
        let v: serde_json::Value = serde_json::from_str(env.stdout_str().trim()).unwrap();
        // Only the alpha+apple row should be removed.
        assert_eq!(v["deleted"].as_u64().unwrap(), 1);
        let conn = db::open(&db).unwrap();
        let beta_apples = db::list(
            &conn,
            Some("beta"),
            None,
            10,
            0,
            None,
            None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(beta_apples.len(), 1);
    }

    #[test]
    fn test_forget_no_filter_errors_or_no_op() {
        // db::forget bails when no filter is supplied. The handler turns
        // that into an stderr line + std::process::exit(1) — which we
        // can't observe in-process. Surface the bail by calling db::forget
        // directly so the test asserts the underlying contract.
        let env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "x", "y");
        let conn = db::open(&db).unwrap();
        let res = db::forget(&conn, None, None, None, false);
        assert!(res.is_err(), "no-filter forget must error");
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("at least one of namespace, pattern, or tier")
        );
    }

    #[test]
    fn test_forget_text_output_count() {
        let mut env = TestEnv::fresh();
        let db = env.db_path.clone();
        let _ = seed_memory(&db, "ns", "a", "x");
        let _ = seed_memory(&db, "ns", "b", "y");
        let mut a = args();
        a.namespace = Some("ns".to_string());
        {
            let mut out = env.output();
            cmd_forget(&db, &a, false, &mut out).unwrap();
        }
        let stdout = env.stdout_str();
        assert!(stdout.contains("forgot 2 memories"), "got: {stdout}");
    }
}
